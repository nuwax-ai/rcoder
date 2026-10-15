//! Real Stop handler and authenticated owner RPC, with a controlled terminal barrier.
//! The fixture is a protocol owner, not a deployed app-cli/Pingap acceptance result.
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use axum::{Router, body::Body, http::Request, response::IntoResponse};
use serde_json::{Value, json};
use shared_types::{
    DesiredState, HttpResult, ObservedHealth, RuntimeIdentityView, RuntimeOperationAccepted,
    RuntimeOperationKind, RuntimeOperationRequest, RuntimeOperationState, RuntimeOperationView,
    RuntimeStatusView,
};
use tokio::sync::Mutex;
use tower::ServiceExt;

#[tokio::test]
async fn accepted_stop_stays_active_after_http_waiter_disconnect_until_original_terminal() {
    stop_activity_case(false, false, false, false).await;
}

#[tokio::test]
async fn unknown_stop_result_keeps_original_activity_until_actual_failed_terminal_without_replay() {
    stop_activity_case(true, false, false, false).await;
}

#[tokio::test]
async fn actual_stop_dispatch_survives_cache_io_failure_until_original_terminal() {
    stop_activity_case(true, true, false, false).await;
}

#[tokio::test]
async fn stop_store_io_failure_before_dispatch_does_not_create_unknown_activity() {
    stop_activity_case(false, false, true, false).await;
}

#[tokio::test]
async fn legacy_original_stop_observed_pending_stays_active_until_terminal_without_new_dispatch() {
    stop_activity_case(true, false, false, true).await;
}

async fn stop_activity_case(
    unknown_result: bool,
    damage_after_dispatch: bool,
    damage_before_dispatch: bool,
    legacy: bool,
) {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("idle-stop");
    std::fs::create_dir_all(&workspace).unwrap();
    let token_root = directory.path().join(".app-cli-state/idle-stop");
    std::fs::create_dir_all(&token_root).unwrap();
    std::fs::write(token_root.join("token"), "controlled-stop-token").unwrap();
    let identity = RuntimeIdentityView {
        application_id: "idle-stop".into(),
        service_family: "userapp-dev".into(),
        workspace_id: "idle-workspace".into(),
        source_root: workspace.display().to_string(),
        runtime_instance_id: "idle-original-owner".into(),
        deployment_generation_id: "idle-generation".into(),
        protocol_version: shared_types::RUNTIME_CONTROL_PROTOCOL_VERSION,
        capabilities: vec![shared_types::OFFICIAL_PROXY_RESTART_CAPABILITY.into()],
    };
    let posts = Arc::new(Mutex::new(Vec::<RuntimeOperationRequest>::new()));
    let terminal = Arc::new(AtomicBool::new(false));
    let terminal_observed = Arc::new(AtomicBool::new(false));
    let admitted = Arc::new(tokio::sync::Notify::new());
    let pending_view_observed = Arc::new(tokio::sync::Notify::new());
    let owner = Router::new()
        .route(
            "/v1/runtime/identity",
            axum::routing::get(move || {
                let identity = identity.clone();
                async move { axum::Json(HttpResult::success(identity)) }
            }),
        )
        .route(
            "/v1/runtime/status",
            axum::routing::get(|| async {
                axum::Json(HttpResult::success(RuntimeStatusView {
                    desired: DesiredState::Running,
                    observed: ObservedHealth::Unknown,
                    active_target: None,
                    revision: 0,
                    active_operation_id: None,
                    recovery_protection: false,
                    runtime_instance_id: "idle-original-owner".into(),
                }))
            }),
        )
        .route(
            "/v1/runtime/operations",
            axum::routing::post({
                let posts = posts.clone();
                let admitted = admitted.clone();
                let cache = directory.path().join("logs/dev-server-external.json");
                move |headers: axum::http::HeaderMap,
                      axum::Json(request): axum::Json<RuntimeOperationRequest>| {
                    let posts = posts.clone();
                    let admitted = admitted.clone();
                    let cache = cache.clone();
                    async move {
                        assert_eq!(headers["x-deploy-token"], "controlled-stop-token");
                        assert_eq!(request.kind, RuntimeOperationKind::Stop);
                        assert_eq!(request.expected_runtime_instance_id, "idle-original-owner");
                        assert_eq!(request.workspace_id, "idle-workspace");
                        posts.lock().await.push(request.clone());
                        if damage_after_dispatch {
                            // This is after the real POST reached the owner,
                            // never a stale file read used to invent activity.
                            std::fs::remove_file(&cache).unwrap();
                            std::fs::create_dir(&cache).unwrap();
                        }
                        admitted.notify_one();
                        (
                            axum::http::StatusCode::ACCEPTED,
                            axum::Json(HttpResult::success(RuntimeOperationAccepted {
                                operation_id: request.operation_id.clone(),
                                state: RuntimeOperationState::Accepted,
                                poll: format!("/v1/runtime/operations/{}", request.operation_id),
                            })),
                        )
                    }
                }
            }),
        )
        .route(
            "/v1/runtime/operations/{id}",
            axum::routing::get({
                let posts = posts.clone();
                let terminal = terminal.clone();
                let terminal_observed = terminal_observed.clone();
                let pending_view_observed = pending_view_observed.clone();
                move |axum::extract::Path(id): axum::extract::Path<String>| {
                    let posts = posts.clone();
                    let terminal = terminal.clone();
                    let terminal_observed = terminal_observed.clone();
                    let pending_view_observed = pending_view_observed.clone();
                    async move {
                        let posts = posts.lock().await;
                        let Some(request) = posts.iter().find(|request| request.operation_id == id)
                        else {
                            return (
                                axum::http::StatusCode::NOT_FOUND,
                                axum::Json(json!({"message":"not admitted"})),
                            )
                                .into_response();
                        };
                        let ended = terminal.load(Ordering::Acquire);
                        pending_view_observed.notify_one();
                        terminal_observed.store(ended, Ordering::Release);
                        axum::Json(HttpResult::success(RuntimeOperationView {
                            operation_id: id,
                            kind: RuntimeOperationKind::Stop,
                            state: if ended {
                                if unknown_result {
                                    RuntimeOperationState::Failed
                                } else {
                                    RuntimeOperationState::Succeeded
                                }
                            } else if unknown_result {
                                RuntimeOperationState::RecoveryRequired
                            } else {
                                RuntimeOperationState::Accepted
                            },
                            request_digest: shared_types::runtime_request_digest(request).unwrap(),
                            revision: 1,
                            runtime_instance_id: "idle-original-owner".into(),
                            error_code: None,
                            error_message: None,
                            failure_detail: None,
                        }))
                        .into_response()
                    }
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let server = tokio::spawn(async move { axum::serve(listener, owner).await.unwrap() });
    if legacy {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let original = RuntimeOperationRequest {
            operation_id: "original-legacy-stop".into(),
            expected_runtime_instance_id: "idle-original-owner".into(),
            expected_revision: 0,
            workspace_id: "idle-workspace".into(),
            kind: RuntimeOperationKind::Stop,
            profile: shared_types::RunProfileInput::Source {
                workspace_id: "idle-workspace".into(),
            },
            run_config: None,
            request_context: None,
        };
        let body = serde_json::to_string(&original).unwrap();
        let mut client = tokio::net::TcpStream::connect(&address).await.unwrap();
        client.write_all(format!("POST /v1/runtime/operations HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\nContent-Type: application/json\r\nX-Deploy-Token: controlled-stop-token\r\nContent-Length: {}\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        let mut reply = Vec::new();
        client.read_to_end(&mut reply).await.unwrap();
        assert!(
            String::from_utf8_lossy(&reply).starts_with("HTTP/1.1 202"),
            "original legacy operation was actually accepted: {reply:?}"
        );
        std::fs::create_dir_all(directory.path().join("logs")).unwrap();
        std::fs::write(directory.path().join("logs/dev-server-external.json"), json!({
            "owners":{"userapp:idle-stop":{"pid":0,"port":9080,"project_id":"userapp:idle-stop",
                "owner":{"address":address,"runtime_instance_id":"idle-original-owner"}}},
            "stops":{"userapp:idle-stop":{"operation_id":"original-legacy-stop","workspace_id":"idle-workspace","submitted_at_ms":1}},
            "intents":{}
        }).to_string()).unwrap();
    }
    let fs = file_server::FileServer::builder(file_server::Config {
        userapp_workspace_dir: directory.path().into(),
        log_base_dir: directory.path().join("logs"),
        app_cli_admin_probe_addr: address,
        app_cli_bin: Some("/must-not-create-a-second-owner".into()),
        ..file_server::Config::default()
    })
    .build()
    .unwrap();
    let state = crate::UserAppState::new(fs.state());
    if damage_before_dispatch {
        let cache = directory.path().join("logs/dev-server-external.json");
        if cache.exists() {
            std::fs::remove_file(&cache).unwrap();
        }
        std::fs::create_dir_all(&cache).unwrap();
    }
    let router = crate::routes::userapp_top_router()
        .split_for_parts()
        .0
        .with_state(state.clone());
    assert_eq!(process_utils::workers::active_in_process(), 0);
    let observer = tokio::spawn(async move {
        router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/userapp/dev/stop")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"app_id":"idle-stop"}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
    });
    if damage_before_dispatch {
        let response = tokio::time::timeout(Duration::from_secs(5), observer)
            .await
            .unwrap()
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let result: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            result["success"], false,
            "write-before-store-failure must fail fast: {result}"
        );
        state
            .build_tasks
            .workers
            .drain(tokio::time::Instant::now() + Duration::from_secs(5))
            .await
            .unwrap();
        assert!(
            posts.lock().await.is_empty(),
            "failed preparation must not POST any Stop"
        );
        assert_eq!(
            process_utils::workers::active_in_process(),
            0,
            "a store error before dispatch cannot manufacture unknown execution"
        );
        server.abort();
        let _server = server.await;
        return;
    }
    tokio::time::timeout(
        Duration::from_secs(5),
        if legacy {
            pending_view_observed.notified()
        } else {
            admitted.notified()
        },
    )
    .await
    .expect("real handler admitted the original Stop");
    let original = posts.lock().await[0].clone();
    let active_before_disconnect = process_utils::workers::active_in_process();
    if unknown_result {
        let response = tokio::time::timeout(Duration::from_secs(5), observer)
            .await
            .unwrap()
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let result: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            result["success"], false,
            "unconfirmed Stop cannot become HTTP success: {result}"
        );
    } else {
        observer.abort();
        let _aborted = observer.await;
    }
    tokio::time::sleep(Duration::from_millis(30)).await;
    let active_after_disconnect = process_utils::workers::active_in_process();
    // Release the original result before asserting the negative baseline, so
    // a failing fixture does not leave an accepted worker behind.
    terminal.store(true, Ordering::Release);
    let cleanup = state
        .build_tasks
        .workers
        .drain(tokio::time::Instant::now() + Duration::from_secs(5))
        .await;
    let same_original = {
        let posts = posts.lock().await;
        posts.len() == 1 && posts[0].operation_id == original.operation_id
    };
    let observed = terminal_observed.load(Ordering::Acquire);
    server.abort();
    let _server = server.await;
    assert!(cleanup.is_ok(), "original Stop worker cleanup: {cleanup:?}");
    assert!(
        active_before_disconnect > 0,
        "accepted Stop was invisible to the real idle activity observation"
    );
    assert!(
        active_after_disconnect > 0,
        "disconnect ended activity before actual Stop completed"
    );
    assert!(
        observed,
        "detached Stop must consume its original actual terminal result"
    );
    assert!(
        same_original,
        "a lost HTTP observer cannot mint/replay a replacement Stop"
    );
    assert_eq!(process_utils::workers::active_in_process(), 0);
}

#[cfg(unix)]
#[tokio::test]
async fn actual_owner_exit_after_captured_stop_cleanup_releases_original_activity_from_durable_result()
 {
    // A real disposable protocol owner and its real child. This tests the
    // platform handler's ownership, not an official app-cli/Pingap deployment.
    const OWNER: &str = r#"
import fcntl,hashlib,http.server,json,os,subprocess,sys,threading,time
base=sys.argv[1]; workspace=os.path.join(base,'exit-stop'); root=os.path.join(base,'.app-cli-state','exit-stop')
lock=open(os.path.join(root,'owner.lock'),'a+'); fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
child=subprocess.Popen(['sleep','3000']); open(os.path.join(base,'business-pid'),'w').write(str(child.pid))
request=None
def envelope(data): return {'success':True,'code':'0','message':'ok','tid':'controlled-owner','data':data}
def view(state):
 c={'v':1,'kind':request['kind'],'profile':request['profile'],'expected_revision':request['expected_revision'],'workspace_id':request['workspace_id']}
 digest=hashlib.sha256(json.dumps(c,separators=(',',':'),ensure_ascii=False).encode()).hexdigest()
 return {'operation_id':request['operation_id'],'kind':'stop','state':state,'request_digest':digest,'revision':request['expected_revision'],'runtime_instance_id':'real-exit-owner','error_code':None,'error_message':None,'failure_detail':None}
class Handler(http.server.BaseHTTPRequestHandler):
 def log_message(self,*args): pass
 def send(self,status,data):
  payload=json.dumps(envelope(data)).encode(); self.send_response(status); self.send_header('Content-Type','application/json'); self.send_header('Content-Length',str(len(payload))); self.end_headers(); self.wfile.write(payload)
 def do_GET(self):
  if self.path=='/v1/runtime/identity': self.send(200,{'application_id':'exit-stop','service_family':'userapp-dev','workspace_id':'exit-workspace','source_root':workspace,'runtime_instance_id':'real-exit-owner','deployment_generation_id':'real-generation','protocol_version':int(sys.argv[2]),'capabilities':[]})
  elif self.path=='/v1/runtime/status': self.send(200,{'desired':'running','observed':'unknown','active_target':None,'revision':0,'active_operation_id':None,'recovery_protection':False,'runtime_instance_id':'real-exit-owner'})
  elif request and self.path=='/v1/runtime/operations/'+request['operation_id']: self.send(200,view('recovery_required'))
  else: self.send(404,None)
 def do_POST(self):
  global request
  assert self.path=='/v1/runtime/operations'; assert self.headers['X-Deploy-Token']=='controlled-stop-token'
  request=json.loads(self.rfile.read(int(self.headers['Content-Length']))); assert request['kind']=='stop'
  with open(os.path.join(base,'posts.log'),'a') as out: out.write(request['operation_id']+'\n')
  open(os.path.join(base,'original-request.json'),'w').write(json.dumps(request))
  self.send(202,{'operation_id':request['operation_id'],'state':'accepted','poll':'/v1/runtime/operations/'+request['operation_id']})
def settle():
 while not os.path.exists(os.path.join(base,'release')): time.sleep(.01)
 child.terminate(); child.wait(timeout=5)
 open(os.path.join(base,'actual-child-exit'),'w').write(str(child.returncode))
 os.makedirs(os.path.join(root,'operations'),exist_ok=True)
 path=os.path.join(root,'operations',request['operation_id']+'.json')
 with open(path+'.tmp','w') as out: json.dump({'request':request,'view':view('succeeded')},out); out.flush(); os.fsync(out.fileno())
 os.replace(path+'.tmp',path); os._exit(0)
server=http.server.ThreadingHTTPServer(('127.0.0.1',0),Handler)
open(os.path.join(base,'address.tmp'),'w').write('127.0.0.1:'+str(server.server_port)); os.replace(os.path.join(base,'address.tmp'),os.path.join(base,'address'))
threading.Thread(target=settle,daemon=True).start(); server.serve_forever()
"#;
    struct OwnedOwner(tokio::process::Child);
    impl Drop for OwnedOwner {
        fn drop(&mut self) {
            if let Some(pid) = self.0.id()
                && matches!(self.0.try_wait(), Ok(None))
            {
                let _cleanup = process_utils::kill_process_group_with_fallback(
                    pid,
                    process_utils::KillSignal::SIGKILL,
                );
            }
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("exit-stop");
    let root = directory.path().join(".app-cli-state/exit-stop");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("token"), "controlled-stop-token").unwrap();
    let mut command = tokio::process::Command::new("python3");
    command
        .args(["-c", OWNER])
        .arg(directory.path())
        .arg(shared_types::RUNTIME_CONTROL_PROTOCOL_VERSION.to_string())
        .kill_on_drop(true)
        .process_group(0);
    let mut owner = OwnedOwner(command.spawn().expect("real protocol-owner process"));
    let address = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(address) = std::fs::read_to_string(directory.path().join("address"))
                && address.parse::<std::net::SocketAddr>().is_ok()
                && tokio::net::TcpStream::connect(&address).await.is_ok()
            {
                break address;
            }
            assert!(
                owner.0.try_wait().unwrap().is_none(),
                "owner exited during setup"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let business_pid: u32 = std::fs::read_to_string(directory.path().join("business-pid"))
        .unwrap()
        .parse()
        .unwrap();
    assert!(process_utils::process_exists(business_pid).unwrap());
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(root.join("owner.lock"))
        .unwrap();
    assert!(
        lock.try_lock().is_err(),
        "actual original owner holds its OS lock"
    );
    let fs = file_server::FileServer::builder(file_server::Config {
        userapp_workspace_dir: directory.path().into(),
        log_base_dir: directory.path().join("logs"),
        app_cli_admin_probe_addr: address,
        app_cli_bin: Some("/must-not-spawn-replacement".into()),
        ..file_server::Config::default()
    })
    .build()
    .unwrap();
    let state = crate::UserAppState::new(fs.state());
    let router = crate::routes::userapp_top_router()
        .split_for_parts()
        .0
        .with_state(state.clone());
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        router.oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/userapp/dev/stop")
                .header("content-type", "application/json")
                .body(Body::from(json!({"app_id":"exit-stop"}).to_string()))
                .unwrap(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let result: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        result["success"], false,
        "pending cleanup is not success: {result}"
    );
    assert!(
        process_utils::workers::active_in_process() > 0,
        "the original Stop remains owned; HTTP={result}; submitted={:?}",
        std::fs::read_to_string(directory.path().join("original-request.json"))
    );
    let original: RuntimeOperationRequest = serde_json::from_slice(
        &std::fs::read(directory.path().join("original-request.json")).unwrap(),
    )
    .unwrap();
    std::fs::write(
        directory.path().join("release"),
        b"release actual captured cleanup",
    )
    .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(5), owner.0.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    assert!(
        !process_utils::process_exists(business_pid).unwrap(),
        "the actual old business child exited"
    );
    lock.try_lock()
        .expect("actual old owner exited and released its OS lock");
    lock.unlock().unwrap();
    state
        .build_tasks
        .workers
        .drain(tokio::time::Instant::now() + Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(
        process_utils::workers::active_in_process(),
        0,
        "a dead captured owner with a verified original terminal result is not permanently active"
    );
    let stored: Value = serde_json::from_slice(
        &std::fs::read(
            root.join("operations")
                .join(format!("{}.json", original.operation_id)),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(stored["view"]["operation_id"], original.operation_id);
    assert_eq!(stored["view"]["state"], "succeeded");
    assert_eq!(
        std::fs::read_to_string(directory.path().join("posts.log"))
            .unwrap()
            .lines()
            .count(),
        1,
        "no replacement Stop is submitted after the original owner exits"
    );
}
