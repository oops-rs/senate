//! Wire fixture for tests of named roster seats (chairs, secretaries, shared-seat
//! routing). These fixtures address fixed senators. Native identity tests use
//! `hook::run_as` or the actual hook executable.
use senate_bridge::{
    hook::{self, HookEnv, HookKind, HookTimeouts},
    ipc::{HookReply, Request},
    session::Runtime,
};
use std::path::Path;

pub async fn hook(
    kind: HookKind,
    socket: &Path,
    input: &str,
    env: &HookEnv,
    runtime: Option<Runtime>,
    timeouts: HookTimeouts,
) -> Option<String> {
    let request = hook::prepare(kind, socket, input, env, runtime, timeouts.git).await?;
    match request {
        Request::SessionHook(report) => {
            let reply: HookReply = senate_bridge::ipc::request(
                socket,
                &Request::Hook(report),
                timeouts.connect,
                timeouts.request,
            )
            .await
            .ok()?;
            reply.context.map(|text| hook::output_for(kind, &text))
        }
        end => {
            let _: serde_json::Value =
                senate_bridge::ipc::request(socket, &end, timeouts.connect, timeouts.request)
                    .await
                    .ok()?;
            None
        }
    }
}
