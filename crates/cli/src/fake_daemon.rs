//! A daemon for unit tests that knows one sandbox and refuses every request that would change
//! a sandbox, so a test sees whether a command asks the daemon about the sandbox before it
//! changes it. It answers an upgrade as up to date with the requested image, so a test sees
//! which image a command asks for.

use std::pin::Pin;
use tokio::net::TcpListener;
use tonic::codegen::tokio_stream::{self, Stream};
use tonic::transport::server::TcpIncoming;
use tonic::transport::{Channel, Server};
use tonic::{Request, Response, Status, Streaming};

use crate::api::sandbox_management_service_client::SandboxManagementServiceClient;
use crate::api::sandbox_management_service_server::{
    SandboxManagementService, SandboxManagementServiceServer,
};
use crate::api::{
    AttachRequest, AttachResponse, ForwardPortRequest, ForwardPortResponse, GetSandboxRequest,
    GetSandboxResponse, ListNetworkActivityRequest, ListNetworkActivityResponse,
    ListSandboxesRequest, ListSandboxesResponse, ListSecretsRequest, ListSecretsResponse,
    NetworkActivity, RemovePortRequest, RemovePortResponse, RemoveSandboxRequest,
    RemoveSandboxResponse, RemoveSecretRequest, RemoveSecretResponse, SandboxUpgraded,
    SetSecretRequest, SetSecretResponse, SshTunnelRequest, SshTunnelResponse, StartSandboxRequest,
    StartSandboxResponse, StopSandboxRequest, StopSandboxResponse, UpdateNetworkRequest,
    UpdateNetworkResponse, UpgradeSandboxRequest, UpgradeSandboxResponse, upgrade_sandbox_response,
};

type BoxStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send>>;

/// Returns the message of the status the fake daemon answers the request `rpc` with, which
/// would change `sandbox`.
pub(crate) fn refusal(rpc: &str, sandbox: &str) -> String {
    format!("refused {rpc} for {sandbox}")
}

/// A daemon with one sandbox, `name`, that mounts `workspace_host_path` and has recorded the
/// network activity `activity`, or has no network log when it's `None`.
struct FakeDaemon {
    name: String,
    workspace_host_path: String,
    activity: Option<Vec<NetworkActivity>>,
}

/// Serves a fake daemon with one sandbox without network activity on a free local port and
/// returns a client for it.
pub(crate) async fn serve(
    name: &str,
    workspace_host_path: &str,
) -> SandboxManagementServiceClient<Channel> {
    serve_with_activity(name, workspace_host_path, Some(Vec::new())).await
}

/// Serves a fake daemon like [`serve`] whose sandbox recorded the network activity, or has no
/// network log when `activity` is `None`.
pub(crate) async fn serve_with_activity(
    name: &str,
    workspace_host_path: &str,
    activity: Option<Vec<NetworkActivity>>,
) -> SandboxManagementServiceClient<Channel> {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let daemon = FakeDaemon {
        name: name.to_string(),
        workspace_host_path: workspace_host_path.to_string(),
        activity,
    };

    tokio::spawn(async move {
        let result = Server::builder()
            .add_service(SandboxManagementServiceServer::new(daemon))
            .serve_with_incoming(TcpIncoming::from(listener))
            .await;

        if let Err(err) = result {
            panic!("the fake daemon stopped serving: {err}");
        }
    });

    let channel = Channel::from_shared(format!("http://{address}"))
        .unwrap()
        .connect_lazy();

    SandboxManagementServiceClient::new(channel)
}

fn refused<T>(rpc: &str, sandbox: &str) -> Result<T, Status> {
    Err(Status::permission_denied(refusal(rpc, sandbox)))
}

#[tonic::async_trait]
impl SandboxManagementService for FakeDaemon {
    type StartSandboxStream = BoxStream<StartSandboxResponse>;
    type AttachStream = BoxStream<AttachResponse>;
    type SshTunnelStream = BoxStream<SshTunnelResponse>;
    type UpgradeSandboxStream = BoxStream<UpgradeSandboxResponse>;

    async fn start_sandbox(
        &self,
        request: Request<StartSandboxRequest>,
    ) -> Result<Response<Self::StartSandboxStream>, Status> {
        refused("start_sandbox", &request.into_inner().name)
    }

    async fn stop_sandbox(
        &self,
        request: Request<StopSandboxRequest>,
    ) -> Result<Response<StopSandboxResponse>, Status> {
        refused("stop_sandbox", &request.into_inner().name)
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
        request: Request<RemoveSandboxRequest>,
    ) -> Result<Response<RemoveSandboxResponse>, Status> {
        refused("remove_sandbox", &request.into_inner().name)
    }

    async fn attach(
        &self,
        _: Request<Streaming<AttachRequest>>,
    ) -> Result<Response<Self::AttachStream>, Status> {
        refused("attach", "")
    }

    async fn ssh_tunnel(
        &self,
        _: Request<Streaming<SshTunnelRequest>>,
    ) -> Result<Response<Self::SshTunnelStream>, Status> {
        refused("ssh_tunnel", "")
    }

    async fn set_secret(
        &self,
        request: Request<SetSecretRequest>,
    ) -> Result<Response<SetSecretResponse>, Status> {
        refused(
            "set_secret",
            &request.into_inner().sandbox.unwrap_or_default(),
        )
    }

    async fn list_secrets(
        &self,
        _: Request<ListSecretsRequest>,
    ) -> Result<Response<ListSecretsResponse>, Status> {
        Ok(Response::new(ListSecretsResponse::default()))
    }

    async fn remove_secret(
        &self,
        request: Request<RemoveSecretRequest>,
    ) -> Result<Response<RemoveSecretResponse>, Status> {
        refused(
            "remove_secret",
            &request.into_inner().sandbox.unwrap_or_default(),
        )
    }

    async fn update_network(
        &self,
        request: Request<UpdateNetworkRequest>,
    ) -> Result<Response<UpdateNetworkResponse>, Status> {
        refused("update_network", &request.into_inner().name)
    }

    async fn forward_port(
        &self,
        request: Request<ForwardPortRequest>,
    ) -> Result<Response<ForwardPortResponse>, Status> {
        refused("forward_port", &request.into_inner().name)
    }

    async fn remove_port(
        &self,
        request: Request<RemovePortRequest>,
    ) -> Result<Response<RemovePortResponse>, Status> {
        refused("remove_port", &request.into_inner().name)
    }

    async fn upgrade_sandbox(
        &self,
        request: Request<UpgradeSandboxRequest>,
    ) -> Result<Response<Self::UpgradeSandboxStream>, Status> {
        let request = request.into_inner();

        if request.name != self.name {
            return Err(Status::not_found("couldn't find specified sandbox"));
        }

        let upgraded = SandboxUpgraded {
            upgraded: false,
            old_image: request.image.clone(),
            new_image: request.image,
        };
        let response = UpgradeSandboxResponse {
            message: Some(upgrade_sandbox_response::Message::Upgraded(upgraded)),
        };

        Ok(Response::new(Box::pin(tokio_stream::iter([Ok(response)]))))
    }

    async fn list_network_activity(
        &self,
        request: Request<ListNetworkActivityRequest>,
    ) -> Result<Response<ListNetworkActivityResponse>, Status> {
        if request.into_inner().name != self.name {
            return Err(Status::not_found("couldn't find specified sandbox"));
        }

        match &self.activity {
            Some(entries) => Ok(Response::new(ListNetworkActivityResponse {
                entries: entries.clone(),
            })),
            None => Err(Status::failed_precondition("no network log")),
        }
    }
}

/// Host directory the fake daemon's sandbox mounts in tests about another workspace.
pub(crate) const OTHER_WORKSPACE: &str = "/home/user/other-project";

/// Start of the error for the sandbox `dev` when it mounts [`OTHER_WORKSPACE`].
pub(crate) const DEV_BELONGS_TO_OTHER: &str = "sandbox dev belongs to /home/user/other-project;";

/// Asserts that the command failed because the sandbox `dev` mounts [`OTHER_WORKSPACE`].
pub(crate) fn assert_dev_belongs_to_other<T: std::fmt::Debug>(result: anyhow::Result<T>) {
    let error = result.unwrap_err().to_string();

    assert!(error.starts_with(DEV_BELONGS_TO_OTHER), "{error}");
}
