//! Real HTTP/Kubernetes client regressions: capture, UID replacement and live consumers.
use super::*;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn runtime(client: kube::Client) -> KubernetesRuntime {
    use crate::runtime::kubernetes_runtime::KubernetesRuntimeConfig;
    KubernetesRuntime {
        client,
        namespace: "review-test".into(),
        config: KubernetesRuntimeConfig {
            namespace: "review-test".into(),
            cluster_domain: "cluster.local".into(),
            pod_ttl_seconds: None,
            image_pull_secret: None,
            service_account_name: "test".into(),
            nfs_server: "unused".into(),
            nfs_path: "/unused".into(),
            storage_class: "unused".into(),
            access_mode: "ReadWriteOnce".into(),
            docker_manager_config: Default::default(),
            kubernetes_config: Default::default(),
            execution_authority: "k8s:test".into(),
        },
        pod_cache: Default::default(),
        subvolume_path_cache: Default::default(),
        event_publisher: Default::default(),
        event_counters: Arc::new(crate::runtime::k8s_event_publisher::PublisherCounters::default()),
    }
}

#[tokio::test]
async fn retained_storage_wire_preserves_original_uid_and_rejects_consumers_or_replacement() {
    tokio::time::timeout(std::time::Duration::from_secs(20),async {
        for scenario in ["healthy","occupied","replaced","controller"] {
            let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address=listener.local_addr().unwrap();
            drop(rustls::crypto::ring::default_provider().install_default());
            let runtime=runtime(kube::Client::try_from(kube::Config::new(format!("http://{address}").parse().unwrap())).unwrap());
            let pvc=Arc::new(Mutex::new(serde_json::json!({"apiVersion":"v1","kind":"PersistentVolumeClaim","metadata":{
                "name":"rcoder-app-51-workspace","uid":"original-volume","resourceVersion":"1",
                "labels":{"app.kubernetes.io/managed-by":"rcoder-runtime","service_type":"user-app"}},
                "spec":{"accessModes":["ReadWriteOnce"],"resources":{"requests":{"storage":"1Gi"}},"storageClassName":"old-class"}})));
            let occupied=Arc::new(AtomicBool::new(false));
            let patches=Arc::new(AtomicUsize::new(0));
            let existing_controller=scenario=="controller";
            let server_pvc=pvc.clone(); let server_occupied=occupied.clone(); let server_patches=patches.clone();
            let server=tokio::spawn(async move {
                loop {
                    let (mut stream,_)=listener.accept().await.unwrap();
                    let mut bytes=Vec::new(); let mut chunk=[0u8;4096];
                    let (head,offset,length)=loop {
                        let count=stream.read(&mut chunk).await.unwrap(); assert!(count>0); bytes.extend_from_slice(&chunk[..count]);
                        if let Some(end)=bytes.windows(4).position(|b|b==b"\r\n\r\n") {
                            let head=String::from_utf8_lossy(&bytes[..end]).into_owned();
                            let length=head.lines().find_map(|line|{let (k,v)=line.split_once(':')?;k.eq_ignore_ascii_case("content-length").then(||v.trim().parse::<usize>().unwrap())}).unwrap_or(0);
                            break (head,end+4,length);
                        }
                    };
                    while bytes.len()<offset+length {let count=stream.read(&mut chunk).await.unwrap();assert!(count>0);bytes.extend_from_slice(&chunk[..count]);}
                    let method=head.split_whitespace().next().unwrap(); let path=head.split_whitespace().nth(1).unwrap().split('?').next().unwrap();
                    let (code,value)=if existing_controller && path.ends_with("/deployments/rcoder-app-51") {
                        (200,serde_json::json!({"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":"rcoder-app-51","uid":"existing-controller"}}))
                    } else if path.ends_with("/persistentvolumeclaims/rcoder-app-51-workspace") {
                        if method=="PATCH" {
                            let patch:serde_json::Value=serde_json::from_slice(&bytes[offset..offset+length]).unwrap();
                            {let mut pvc=server_pvc.lock().unwrap();
                                assert_eq!(patch["metadata"]["uid"],pvc["metadata"]["uid"]);
                                assert_eq!(patch["metadata"]["resourceVersion"],pvc["metadata"]["resourceVersion"]);
                                pvc["metadata"]["annotations"]=patch["metadata"]["annotations"].clone();
                                pvc["metadata"]["resourceVersion"]="2".into();}
                            server_patches.fetch_add(1,Ordering::SeqCst);
                        } else {assert_eq!(method,"GET");}
                        (200,server_pvc.lock().unwrap().clone())
                    } else if path.ends_with("/services/rcoder-app-51-svc") {
                        assert_eq!(method,"GET");
                        (200,serde_json::json!({"apiVersion":"v1","kind":"Service","metadata":{"name":"rcoder-app-51-svc","uid":"original-service","resourceVersion":"3",
                            "labels":{"app.kubernetes.io/managed-by":"rcoder-app-manager","rcoder.io/app-id":"51"},
                            "annotations":{"rcoder.io/application-id":"51","rcoder.io/lifecycle-id":"original-life"}},
                            "spec":{"selector":{"rcoder.io/app-id":"51"},"ports":[{"port":9080}]}}))
                    } else if path.ends_with("/pods") {
                        assert_eq!(method,"GET");
                        let items=if server_occupied.load(Ordering::SeqCst){vec![serde_json::json!({"metadata":{"name":"old-pod"},"spec":{"containers":[{"name":"old","image":"fixture"}],"volumes":[{"name":"workspace","persistentVolumeClaim":{"claimName":"rcoder-app-51-workspace"}}]}})]}else{vec![]};
                        (200,serde_json::json!({"apiVersion":"v1","kind":"PodList","items":items}))
                    } else {
                        assert_eq!(method,"GET","Recovery must not delete or recreate a volume: {head}");
                        (404,serde_json::json!({"apiVersion":"v1","kind":"Status","status":"Failure","reason":"NotFound","message":"absent","code":404}))
                    };
                    let body=value.to_string();let response=format!("HTTP/1.1 {code} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len());
                    stream.write_all(response.as_bytes()).await.unwrap();
                }
            });
            let captured=container_runtime_api::UserAppDeploymentRuntime::discover_retained_app_storage(&runtime,"51",UserAppOperationScope::Prod).await.unwrap();
            if existing_controller {
                assert!(captured.is_none(),"ordinary owned compute must not enter retained-storage fallback");
                assert_eq!(patches.load(Ordering::SeqCst),0);
                server.abort();continue;
            }
            let captured=captured.unwrap();
            assert_eq!(captured.lifecycle_id.as_deref(),Some("original-life"));
            assert_eq!(captured.volumes[0].uid,"original-volume");
            assert_eq!(captured.services[0].uid,"original-service");
            if scenario=="occupied" {occupied.store(true,Ordering::SeqCst);}
            if scenario=="replaced" {pvc.lock().unwrap()["metadata"]["uid"]="replacement-volume".into();}
            let context=UserAppExecutionContext{app_id:"51".into(),lifecycle_id:"original-life".into(),operation_id:"recover".into(),executor_id:"worker".into(),request_fingerprint:"a".repeat(64)};
            let result=runtime.recover_retained_storage(&context,&captured).await;
            if scenario=="healthy" {
                result.unwrap();assert_eq!(patches.load(Ordering::SeqCst),1);
                let value=pvc.lock().unwrap();assert_eq!(value["metadata"]["uid"],"original-volume");
                assert_eq!(value["metadata"]["annotations"]["rcoder.io/lifecycle-id"],"original-life");
                assert_eq!(value["spec"]["storageClassName"],"old-class");
                assert_eq!(value["spec"]["resources"]["requests"]["storage"],"1Gi");
            } else {assert!(result.is_err());assert_eq!(patches.load(Ordering::SeqCst),0);}
            server.abort();
        }
    }).await.unwrap();
}
