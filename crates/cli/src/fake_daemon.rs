//! A daemon for unit tests that knows one sandbox and refuses every request that would change
//! a sandbox, so a test sees whether a command asks the daemon about the sandbox before it
//! changes it.

use std::pin::Pin;
use tokio::net::TcpListener;
use tonic::codegen::tokio_stream::Stream;
use tonic::transport::server::TcpIncoming;
use tonic::transport::{Channel, Server};
use tonic::{Request, Response, Status, Streaming};

use crate::api::sandbox_management_service_client::SandboxManagementServiceClient;
use crate::api::sandbox_management_service_server::{
    SandboxManagementService, SandboxManagementServiceServer,
};
use crate::api::{
    AttachRequest, AttachResponse, ForwardPortRequest, ForwardPortResponse, GetSandboxRequest,
    GetSandboxResponse, ListSandboxesRequest, ListSandboxesResponse, ListSecretsRequest,
    ListSecretsResponse, RemovePortRequest, RemovePortResponse, RemoveSandboxRequest,
    RemoveSandboxResponse, RemoveSecretRequest, RemoveSecretResponse, SetSecretRequest,
    SetSecretResponse, SshTunnelRequest, SshTunnelResponse, StartSandboxRequest,
    StartSandboxResponse, StopSandboxRequest, StopSandboxResponse, UpdateNetworkRequest,
    UpdateNetworkResponse,
};

type BoxStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send>>;

/// Message of the status the fake daemon answers every request with that changes a sandbox.
pub(crate) const REFUSED: &str = "the fake daemon doesn't change sandboxes";

/// A daemon with one sandbox, `name`, that mounts `workspace_host_path`.
struct FakeDaemon {
    name: String,
    workspace_host_path: String,
}

/// Serves a fake daemon with one sandbox on a free local port and returns a client for it.
pub(crate) async fn serve(
    name: &str,
    workspace_host_path: &str,
) -> SandboxManagementServiceClient<Channel> {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let daemon = FakeDaemon {
        name: name.to_string(),
        workspace_host_path: workspace_host_path.to_string(),
    };

    tokio::spawn(
        Server::builder()
            .add_service(SandboxManagementServiceServer::new(daemon))
            .serve_with_incoming(TcpIncoming::from(listener)),
    );

    let channel = Channel::from_shared(format!("http://{address}"))
        .unwrap()
        .connect_lazy();

    SandboxManagementServiceClient::new(channel)
}

fn refused<T>() -> Result<T, Status> {
    Err(Status::permission_denied(REFUSED))
}

#[tonic::async_trait]
impl SandboxManagementService for FakeDaemon {
    type StartSandboxStream = BoxStream<StartSandboxResponse>;
    type AttachStream = BoxStream<AttachResponse>;
    type SshTunnelStream = BoxStream<SshTunnelResponse>;

    async fn start_sandbox(
        &self,
        _: Request<StartSandboxRequest>,
    ) -> Result<Response<Self::StartSandboxStream>, Status> {
        refused()
    }

    async fn stop_sandbox(
        &self,
        _: Request<StopSandboxRequest>,
    ) -> Result<Response<StopSandboxResponse>, Status> {
        refused()
    }

    async fn list_sandboxes(
        &self,
        _: Request<ListSandboxesRequest>,
    ) -> Result<Response<ListSandboxesResponse>, Status> {
        Ok(Response::new(ListSandboxesResponse::default()))
    }

    async fn get_sandbox(
        &self,
        request: Request<GetSandboxRequest>,
    ) -> Result<Response<GetSandboxResponse>, Status> {
        if request.into_inner().name != self.name {
            return Err(Status::not_found("no such sandbox"));
        }

        Ok(Response::new(GetSandboxResponse {
            name: self.name.clone(),
            workspace_host_path: self.workspace_host_path.clone(),
            ..Default::default()
        }))
    }

    async fn remove_sandbox(
        &self,
        _: Request<RemoveSandboxRequest>,
    ) -> Result<Response<RemoveSandboxResponse>, Status> {
        refused()
    }

    async fn attach(
        &self,
        _: Request<Streaming<AttachRequest>>,
    ) -> Result<Response<Self::AttachStream>, Status> {
        refused()
    }

    async fn ssh_tunnel(
        &self,
        _: Request<Streaming<SshTunnelRequest>>,
    ) -> Result<Response<Self::SshTunnelStream>, Status> {
        refused()
    }

    async fn set_secret(
        &self,
        _: Request<SetSecretRequest>,
    ) -> Result<Response<SetSecretResponse>, Status> {
        refused()
    }

    async fn list_secrets(
        &self,
        _: Request<ListSecretsRequest>,
    ) -> Result<Response<ListSecretsResponse>, Status> {
        Ok(Response::new(ListSecretsResponse::default()))
    }

    async fn remove_secret(
        &self,
        _: Request<RemoveSecretRequest>,
    ) -> Result<Response<RemoveSecretResponse>, Status> {
        refused()
    }

    async fn update_network(
        &self,
        _: Request<UpdateNetworkRequest>,
    ) -> Result<Response<UpdateNetworkResponse>, Status> {
        refused()
    }

    async fn forward_port(
        &self,
        _: Request<ForwardPortRequest>,
    ) -> Result<Response<ForwardPortResponse>, Status> {
        refused()
    }

    async fn remove_port(
        &self,
        _: Request<RemovePortRequest>,
    ) -> Result<Response<RemovePortResponse>, Status> {
        refused()
    }
}
