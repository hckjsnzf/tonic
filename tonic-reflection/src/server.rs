use crate::proto::server_reflection_request::MessageRequest;
use crate::proto::server_reflection_response::MessageResponse;
use crate::proto::server_reflection_server::{ServerReflection, ServerReflectionServer};
use crate::proto::{
    FileDescriptorResponse, ListServiceResponse, ServerReflectionRequest, ServerReflectionResponse,
    ServiceResponse,
};
use prost::Message;
use prost_types::{
    DescriptorProto, EnumDescriptorProto, FieldDescriptorProto, FileDescriptorProto,
    FileDescriptorSet,
};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use thiserror::Error;
use tokio::stream::StreamExt;
use tokio::sync::mpsc;
use tonic::{Request, Response, Status, Streaming};

/// Represents an error in the construction of a gRPC Reflection Service.
#[derive(Debug, Error)]
pub enum Error {
    /// An error was encountered decoding a `prost_types::FileDescriptorSet` from a buffer.
    #[error("error decoding FileDescriptorProto from buffer")]
    DecodeError(#[from] prost::DecodeError),
    /// A `prost_types::FileDescriptorProto`
    #[error("invalid FileDescriptorSet - {0}")]
    InvalidFileDescriptorSet(String),
}

/// A builder used to construct a gRPC Reflection Service.
#[derive(Debug)]
pub struct Builder {
    file_descriptor_sets: Vec<FileDescriptorSet>,
    encoded_file_descriptor_sets: Vec<&'static [u8]>,
    include_reflection_service: bool,

    service_names: Vec<String>,
    symbols: HashMap<String, Arc<FileDescriptorProto>>,
}

impl Builder {
    /// Create a new builder that can configure a gRPC Reflection Service.
    pub fn configure() -> Self {
        Builder {
            file_descriptor_sets: Vec::new(),
            encoded_file_descriptor_sets: Vec::new(),
            include_reflection_service: true,

            service_names: Vec::new(),
            symbols: HashMap::new(),
        }
    }

    /// Registers an instance of `prost_types::FileDescriptorSet` with the gRPC Reflection
    /// Service builder.
    pub fn register_file_descriptor_set(mut self, file_descriptor_set: FileDescriptorSet) -> Self {
        self.file_descriptor_sets.push(file_descriptor_set);
        self
    }

    /// Registers a byte slice containing an encoded `prost_types::FileDescriptorSet` with
    /// the gRPC Reflection Service builder.
    pub fn register_encoded_file_descriptor_set(
        mut self,
        encoded_file_descriptor_set: &'static [u8],
    ) -> Self {
        self.encoded_file_descriptor_sets
            .push(encoded_file_descriptor_set);
        self
    }

    /// Serve the gRPC Refection Service descriptor via the Reflection Service. This is enabled
    /// by default - set `include` to false to disable.
    pub fn include_reflection_service(mut self, include: bool) -> Self {
        self.include_reflection_service = include;
        self
    }

    /// Build a gRPC Reflection Service to be served via Tonic.
    pub fn build(mut self) -> Result<ServerReflectionServer<impl ServerReflection>, Error> {
        if self.include_reflection_service {
            self = self.register_encoded_file_descriptor_set(crate::proto::FILE_DESCRIPTOR_SET);
        }

        for encoded in &self.encoded_file_descriptor_sets {
            let decoded = FileDescriptorSet::decode(*encoded)?;
            self.file_descriptor_sets.push(decoded);
        }

        let all_fds = self.file_descriptor_sets.clone();
        let mut processed: HashSet<String> = HashSet::new();
        for fds in all_fds {
            for fd in fds.file {
                let name = match fd.name.clone() {
                    None => {
                        return Err(Error::InvalidFileDescriptorSet("missing name".to_string()));
                    }
                    Some(n) => n,
                };

                if processed.contains(&name) {
                    continue;
                }
                processed.insert(name);

                self.process_file(fd.clone())?;
            }
        }

        let service_names = self
            .service_names
            .iter()
            .map(|name| ServiceResponse { name: name.clone() })
            .collect();

        Ok(ServerReflectionServer::new(ReflectionService {
            state: Arc::new(ReflectionServiceState {
                service_names,
                symbols: self.symbols,
            }),
        }))
    }

    fn process_file(&mut self, fd: FileDescriptorProto) -> Result<(), Error> {
        let fd = Arc::new(fd);
        let prefix = &fd.package.clone().unwrap_or_default();

        for msg in &fd.message_type {
            self.process_message(fd.clone(), &prefix, msg)?;
        }

        for en in &fd.enum_type {
            self.process_enum(fd.clone(), &prefix, en)?;
        }

        for extension in &fd.extension {
            self.process_field(fd.clone(), &prefix, &extension)?;
        }

        for service in &fd.service {
            let service_name = extract_name(&prefix, "service", service.name.as_ref())?;
            self.service_names.push(service_name.clone());
            self.symbols.insert(service_name.clone(), fd.clone());

            for method in &service.method {
                let method_name = extract_name(&service_name, "method", method.name.as_ref())?;
                self.symbols.insert(method_name, fd.clone());
            }
        }

        Ok(())
    }

    fn process_message(
        &mut self,
        fd: Arc<FileDescriptorProto>,
        prefix: &str,
        msg: &DescriptorProto,
    ) -> Result<(), Error> {
        let message_name = extract_name(prefix, "message", msg.name.as_ref())?;
        self.symbols.insert(message_name.clone(), fd.clone());

        for nested in &msg.nested_type {
            self.process_message(fd.clone(), &message_name, nested)?;
        }

        for en in &msg.enum_type {
            self.process_enum(fd.clone(), &message_name, en)?;
        }

        for field in &msg.field {
            let field_name = extract_name(&message_name, "field", field.name.as_ref())?;
            self.symbols.insert(field_name, fd.clone());
        }

        for oneof in &msg.oneof_decl {
            let oneof_name = extract_name(&message_name, "oneof", oneof.name.as_ref())?;
            self.symbols.insert(oneof_name, fd.clone());
        }

        Ok(())
    }

    fn process_enum(
        &mut self,
        fd: Arc<FileDescriptorProto>,
        prefix: &str,
        en: &EnumDescriptorProto,
    ) -> Result<(), Error> {
        let enum_name = extract_name(prefix, "enum", en.name.as_ref())?;
        self.symbols.insert(enum_name.clone(), fd.clone());

        for value in &en.value {
            let value_name = extract_name(&enum_name, "enum value", value.name.as_ref())?;
            self.symbols.insert(value_name, fd.clone());
        }

        Ok(())
    }

    fn process_field(
        &mut self,
        fd: Arc<FileDescriptorProto>,
        prefix: &str,
        field: &FieldDescriptorProto,
    ) -> Result<(), Error> {
        let field_name = extract_name(prefix, "field", field.name.as_ref())?;
        self.symbols.insert(field_name, fd.clone());
        Ok(())
    }
}

fn extract_name(
    prefix: &str,
    name_type: &str,
    maybe_name: Option<&String>,
) -> Result<String, Error> {
    match maybe_name {
        None => Err(Error::InvalidFileDescriptorSet(format!(
            "missing {} name",
            name_type
        ))),
        Some(name) => {
            if prefix.is_empty() {
                Ok(name.to_string())
            } else {
                Ok(format!("{}.{}", prefix, name))
            }
        }
    }
}

#[derive(Debug)]
struct ReflectionServiceState {
    service_names: Vec<ServiceResponse>,
    symbols: HashMap<String, Arc<FileDescriptorProto>>,
}

impl ReflectionServiceState {
    fn list_services(&self) -> MessageResponse {
        MessageResponse::ListServicesResponse(ListServiceResponse {
            service: self.service_names.clone(),
        })
    }

    fn symbol_by_name(&self, symbol: &str) -> Result<MessageResponse, Status> {
        match self.symbols.get(symbol) {
            None => Err(Status::not_found(format!("symbol '{}' not found", symbol))),
            Some(fd) => {
                let mut encoded_fd = Vec::new();
                if let Err(_) = fd.clone().encode(&mut encoded_fd) {
                    return Err(Status::internal("encoding error"));
                };

                Ok(MessageResponse::FileDescriptorResponse(
                    FileDescriptorResponse {
                        file_descriptor_proto: vec![encoded_fd],
                    },
                ))
            }
        }
    }
}

#[derive(Debug)]
struct ReflectionService {
    state: Arc<ReflectionServiceState>,
}

#[tonic::async_trait]
impl ServerReflection for ReflectionService {
    type ServerReflectionInfoStream = mpsc::Receiver<Result<ServerReflectionResponse, Status>>;

    async fn server_reflection_info(
        &self,
        req: Request<Streaming<ServerReflectionRequest>>,
    ) -> Result<Response<Self::ServerReflectionInfoStream>, Status> {
        let mut req_rx = req.into_inner();
        let (mut resp_tx, resp_rx) = mpsc::channel::<Result<ServerReflectionResponse, Status>>(1);

        let state = self.state.clone();

        tokio::spawn(async move {
            while let Some(req) = req_rx.next().await {
                let req = match req {
                    Ok(req) => req,
                    Err(_) => {
                        return;
                    }
                };

                let resp_msg = match req.message_request.clone() {
                    None => Err(Status::invalid_argument("invalid MessageRequest")),
                    Some(msg) => match msg {
                        MessageRequest::FileByFilename(_) => unimplemented!(),
                        MessageRequest::FileContainingSymbol(s) => state.symbol_by_name(&s),
                        MessageRequest::FileContainingExtension(_) => unimplemented!(),
                        MessageRequest::AllExtensionNumbersOfType(_) => unimplemented!(),
                        MessageRequest::ListServices(_) => Ok(state.list_services()),
                    },
                };

                match resp_msg {
                    Ok(resp_msg) => {
                        let resp = ServerReflectionResponse {
                            valid_host: req.host.clone(),
                            original_request: Some(req.clone()),
                            message_response: Some(resp_msg),
                        };
                        resp_tx.send(Ok(resp)).await.expect("send");
                    }
                    Err(status) => {
                        resp_tx.send(Err(status)).await.expect("send");
                        return;
                    }
                }
            }
        });

        Ok(Response::new(resp_rx))
    }
}
