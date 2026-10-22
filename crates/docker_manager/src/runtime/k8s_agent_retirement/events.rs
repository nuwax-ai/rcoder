//! Controllers provide watch recovery/relist; events only trigger authoritative inventory.
use super::super::kubernetes_runtime::{KubernetesRuntime, RUNTIME_MANAGED_LABEL};
use container_runtime_api::{AgentResourceEvent, AgentResourceEvents};
use futures_util::StreamExt;
use k8s_openapi::api::{
    apps::v1::StatefulSet,
    core::v1::{Pod, Service},
};
use kube::{
    Api, Resource,
    runtime::{Controller, controller::Action, watcher},
};
use serde::de::DeserializeOwned;
use std::{convert::Infallible, fmt::Debug, sync::Arc, time::Duration};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

struct Context {
    sender: mpsc::Sender<AgentResourceEvent>,
    stop: CancellationToken,
    interval: Duration,
}

async fn reconcile<K>(_resource: Arc<K>, context: Arc<Context>) -> Result<Action, Infallible> {
    tokio::select! {
        _ = context.stop.cancelled() => {},
        result = context.sender.send(AgentResourceEvent::Changed) => {
            if result.is_err() { context.stop.cancel(); }
        },
    }
    Ok(Action::requeue(context.interval))
}

fn policy<K>(_resource: Arc<K>, error: &Infallible, _context: Arc<Context>) -> Action {
    match *error {}
}

async fn controller_ended(context: &Context) {
    if !context.stop.is_cancelled() {
        tokio::select! {
            _ = context.stop.cancelled() => {},
            _result = context.sender.send(AgentResourceEvent::ObservationFailed(
                "Agent resource Controller ended unexpectedly".into(),
            )) => {},
        }
        context.stop.cancel();
    }
}

async fn run<K>(api: Api<K>, context: Arc<Context>)
where
    K: Resource<DynamicType = ()> + Clone + DeserializeOwned + Debug + Send + Sync + 'static,
{
    let mut stream = Controller::new(
        api,
        watcher::Config::default().labels(RUNTIME_MANAGED_LABEL),
    )
    .graceful_shutdown_on(context.stop.clone().cancelled_owned())
    .run(reconcile::<K>, policy::<K>, context.clone())
    .boxed();
    loop {
        // A Controller can still be awaiting its initial reflector store when
        // graceful shutdown closes the input queue. Cancel that pending poll
        // too; the three owned run futures are still joined by the caller.
        let result = tokio::select! {
            biased;
            () = context.stop.cancelled() => break,
            result = stream.next() => match result {
                Some(result) => result,
                None => break,
            },
        };
        if let Err(error) = result {
            tokio::select! {
                _ = context.stop.cancelled() => {},
                result = context.sender.send(AgentResourceEvent::ObservationFailed(error.to_string())) => {
                    if result.is_err() { context.stop.cancel(); }
                },
            }
        }
    }
    drop(stream);
    controller_ended(&context).await;
}

impl KubernetesRuntime {
    pub(in super::super) fn watch_agent_resources(
        &self,
        shutdown: CancellationToken,
        interval: Duration,
    ) -> AgentResourceEvents {
        let (sender, receiver) = mpsc::channel(32);
        let stop = CancellationToken::new();
        let context = Arc::new(Context {
            sender: sender.clone(),
            stop: stop.clone(),
            interval,
        });
        let controllers: Api<StatefulSet> = Api::namespaced(self.client.clone(), &self.namespace);
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let services: Api<Service> = Api::namespaced(self.client.clone(), &self.namespace);
        let task = tokio::spawn(async move {
            let all = async {
                futures_util::future::join3(
                    run(controllers, context.clone()),
                    run(pods, context.clone()),
                    run(services, context),
                )
                .await;
            };
            tokio::pin!(all);
            tokio::select! {
                () = &mut all => {},
                () = shutdown.cancelled() => { stop.cancel(); all.await; },
                () = sender.closed() => { stop.cancel(); all.await; },
            }
        });
        AgentResourceEvents { receiver, task }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::task::Poll;

    #[tokio::test]
    async fn controller_eof_with_full_live_receiver_does_not_block_shutdown() {
        let (sender, mut receiver) = mpsc::channel(1);
        sender
            .send(AgentResourceEvent::Changed)
            .await
            .expect("fill the event channel");
        let context = Context {
            sender,
            stop: CancellationToken::new(),
            interval: Duration::from_secs(1),
        };
        let ending = controller_ended(&context);
        tokio::pin!(ending);

        // Drive the real EOF path until it is blocked on channel capacity.
        assert!(matches!(
            futures_util::poll!(ending.as_mut()),
            Poll::Pending
        ));
        context.stop.cancel();
        tokio::time::timeout(Duration::from_secs(1), ending)
            .await
            .expect("root cancellation must release EOF reporting");

        assert!(matches!(
            receiver.try_recv(),
            Ok(AgentResourceEvent::Changed)
        ));
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert!(context.stop.is_cancelled());
    }
}
