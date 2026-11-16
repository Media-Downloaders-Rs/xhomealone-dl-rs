use personal_downloaders::{Cancel, browser::Cdp};
#[allow(dead_code)]
mod support;
use serde_json::{Value, json};
use std::{net::TcpListener, thread, time::Duration};

fn server(
    handler: impl FnOnce(tungstenite::WebSocket<std::net::TcpStream>) + Send + 'static,
) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("ws://{}/devtools/page/test", listener.local_addr().unwrap());
    let task =
        thread::spawn(move || handler(tungstenite::accept(listener.accept().unwrap().0).unwrap()));
    (url, task)
}

#[test]
fn interleaved_events_and_other_response_ids_do_not_replace_the_result() {
    let (url, task) = server(|mut ws| {
        let request: Value = serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap();
        for value in [
            json!({"method":"Page.loadEventFired","params":{}}),
            json!({"id":999,"result":{"wrong":true}}),
            json!({"id":request["id"],"result":{"answer":42}}),
        ] {
            ws.send(tungstenite::Message::Text(value.to_string().into()))
                .unwrap();
        }
    });
    let mut cdp = Cdp::connect(&url, Cancel::default(), Duration::from_secs(1)).unwrap();
    assert_eq!(
        cdp.call("Test.method", json!({})).unwrap(),
        json!({"answer":42})
    );
    task.join().unwrap();
}

#[test]
fn javascript_exceptions_are_reported_without_leaking_page_secrets() {
    let (url, task) = server(|mut ws| {
        let request: Value = serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap();
        ws.send(tungstenite::Message::Text(json!({"id":request["id"],"result":{"exceptionDetails":{"text":"secret-cookie-value"}}}).to_string().into())).unwrap();
    });
    let mut cdp = Cdp::connect(&url, Cancel::default(), Duration::from_secs(1)).unwrap();
    let error = cdp
        .evaluate("throw 'secret-cookie-value'")
        .unwrap_err()
        .to_string();
    assert!(error.contains("JavaScript"));
    assert!(!error.contains("secret-cookie-value"));
    task.join().unwrap();
}

#[test]
fn closed_browser_connection_is_a_recoverable_error() {
    use personal_downloaders::browser::BrowserDisconnected;
    let (url, task) = server(|mut ws| {
        ws.read().unwrap();
        ws.close(None).unwrap();
    });
    let mut cdp = Cdp::connect(&url, Cancel::default(), Duration::from_secs(1)).unwrap();
    let error = cdp.call("Test.method", json!({})).unwrap_err();
    assert!(error.downcast_ref::<BrowserDisconnected>().is_some());
    task.join().unwrap();
}

#[test]
fn missing_response_times_out() {
    let (url, task) = server(|mut ws| {
        ws.read().unwrap();
        thread::sleep(Duration::from_millis(250));
    });
    let mut cdp = Cdp::connect(&url, Cancel::default(), Duration::from_millis(80)).unwrap();
    assert!(
        cdp.call("Test.method", json!({}))
            .unwrap_err()
            .to_string()
            .contains("Tiempo")
    );
    task.join().unwrap();
}

#[test]
fn waiting_for_response_is_cancellable() {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let (url, task) = server(move |mut ws| {
        ws.read().unwrap();
        ready_tx.send(()).unwrap();
        release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    });
    let cancel = Cancel::default();
    let token = cancel.clone();
    let mut cdp = Cdp::connect(&url, cancel, Duration::from_secs(3)).unwrap();
    let cancel_task = thread::spawn(move || {
        ready_rx.recv().unwrap();
        token.cancel();
    });
    assert!(
        cdp.call("Test.method", json!({}))
            .unwrap_err()
            .to_string()
            .contains("Cancelado")
    );
    cancel_task.join().unwrap();
    release_tx.send(()).unwrap();
    task.join().unwrap();
}

#[test]
#[ignore = "launches an installed browser using a disposable profile"]
fn private_browser_is_usable_and_its_profile_is_removed_on_close() {
    use personal_downloaders::browser::BrowserSession;
    let unrelated = tempfile::tempdir().unwrap();
    let marker = unrelated.path().join("personal-profile-sentinel");
    std::fs::write(&marker, b"preserve").unwrap();
    let profile;
    {
        let mut session =
            BrowserSession::launch(true, Cancel::default(), std::sync::Arc::new(|_| {})).unwrap();
        profile = session.profile_path().to_path_buf();
        session
            .block_urls(&[
                "*xhomealone.com/remote_control.php*",
                "*xhomealone.com/tags/*",
            ])
            .unwrap();
        assert!(profile.is_dir());
        assert_ne!(profile, unrelated.path());
        assert_eq!(session.evaluate("1 + 1").unwrap(), json!(2));
        assert!(
            session
                .cookies("https://xhomealone.com/")
                .unwrap()
                .is_empty()
        );
    }
    assert!(!profile.exists());
    assert_eq!(std::fs::read(marker).unwrap(), b"preserve");
}

#[test]
#[ignore = "launches a visible installed browser with a disposable profile"]
fn visible_private_browser_starts_repeatedly_without_a_signin_page() {
    use personal_downloaders::browser::BrowserSession;
    for _ in 0..3 {
        let profile;
        {
            let mut browser =
                BrowserSession::launch(false, Cancel::default(), std::sync::Arc::new(|_| {}))
                    .unwrap();
            profile = browser.profile_path().to_path_buf();
            assert_eq!(
                browser.evaluate("location.href").unwrap(),
                json!("about:blank")
            );
            assert_eq!(browser.evaluate("6 * 7").unwrap(), json!(42));
        }
        assert!(!profile.exists());
    }
}

#[test]
#[ignore = "launches a dedicated browser twice with a disposable test directory"]
fn dedicated_browser_profile_is_reused_and_never_deletes_its_data() {
    use personal_downloaders::browser::BrowserSession;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("dedicated");
    let first;
    {
        let mut browser = BrowserSession::launch_dedicated(
            &path,
            true,
            Cancel::default(),
            std::sync::Arc::new(|_| {}),
        )
        .unwrap();
        first = browser.profile_path().to_path_buf();
        std::fs::write(first.join("test-profile-marker"), b"keep").unwrap();
        assert_eq!(browser.evaluate("7 * 6").unwrap(), json!(42));
    }
    assert_eq!(
        std::fs::read(first.join("test-profile-marker")).unwrap(),
        b"keep"
    );
    let mut browser = BrowserSession::launch_dedicated(
        &path,
        true,
        Cancel::default(),
        std::sync::Arc::new(|_| {}),
    )
    .unwrap();
    assert_eq!(browser.profile_path(), first);
    assert_eq!(browser.evaluate("6 * 7").unwrap(), json!(42));
}

#[test]
#[ignore = "compares the installed browser context with a normal private launch"]
fn ordinary_browser_debug_port_does_not_enable_the_automation_mode() {
    use personal_downloaders::browser::BrowserSession;
    let mut browser =
        BrowserSession::launch(false, Cancel::default(), std::sync::Arc::new(|_| {})).unwrap();
    assert_eq!(
        browser.evaluate("navigator.webdriver").unwrap(),
        json!(false)
    );
}

#[test]
#[ignore = "uses an installed browser and a local service-worker/cache fixture"]
fn fresh_checks_bypass_both_http_cache_and_stale_service_worker_responses() {
    use personal_downloaders::browser::BrowserSession;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let changed = Arc::new(AtomicBool::new(false));
    let version = changed.clone();
    let fixture = support::http::Server::new(move |request| {
        let (body,kind,cache)=match request.path.as_str() {
            "/worker.js"=>(b"self.addEventListener('install',e=>self.skipWaiting());self.addEventListener('activate',e=>e.waitUntil(clients.claim()));self.addEventListener('fetch',e=>{if(new URL(e.request.url).pathname==='/snapshot')e.respondWith(Promise.resolve(new Response('worker-old')))});".as_slice(),"application/javascript","no-store"),
            "/snapshot"=>(if version.load(Ordering::SeqCst) {b"new".as_slice()} else {b"old".as_slice()},"text/plain","public,max-age=3600"),
            _=>(b"<!doctype html><title>Local cache fixture</title>".as_slice(),"text/html","no-store"),
        };
        support::http::Response {
            status: 200,
            body: body.to_vec(),
            headers: vec![
                ("Content-Type".into(), kind.into()),
                ("Cache-Control".into(), cache.into()),
            ],
            declared: None,
            slow: false,
        }
    });
    let mut browser = BrowserSession::launch(true, Cancel::default(), Arc::new(|_| {})).unwrap();
    let url = format!("{}/", fixture.base);
    let _ = browser.evaluate(&format!(
        "location.href={}",
        serde_json::to_string(&url).unwrap()
    ));
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if browser.evaluate("location.href").ok() == Some(json!(url)) {
            break;
        }
        assert!(std::time::Instant::now() < deadline);
        thread::sleep(Duration::from_millis(30));
    }
    let read = "(async()=>await(await fetch('/snapshot')).text())()";
    assert_eq!(browser.evaluate(read).unwrap(), json!("old"));
    changed.store(true, Ordering::SeqCst);
    assert_eq!(browser.evaluate(read).unwrap(), json!("old"));
    assert_eq!(browser.evaluate("(async()=>{await navigator.serviceWorker.register('/worker.js');await navigator.serviceWorker.ready;for(let i=0;i<100&&!navigator.serviceWorker.controller;i++)await new Promise(r=>setTimeout(r,20));return !!navigator.serviceWorker.controller})()").unwrap(),json!(true));
    assert_eq!(browser.evaluate(read).unwrap(), json!("worker-old"));
    browser.bypass_document_cache().unwrap();
    assert_eq!(browser.evaluate(read).unwrap(), json!("new"));
}
