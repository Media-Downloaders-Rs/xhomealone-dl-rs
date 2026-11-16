use personal_downloaders::transport::{Transient, network_error};
use std::{net::TcpListener, time::Duration};

#[tokio::test]
async fn response_timeout_names_the_host_without_exposing_private_request_data() {
    // A listening socket accepts TCP into its backlog, but never sends HTTP.
    let socket = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!(
        "http://private-user:private-password@{}/private-path?token=private-token",
        socket.local_addr().unwrap()
    );
    let error = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_millis(80))
        .build()
        .unwrap()
        .get(url)
        .header("Cookie", "private-cookie")
        .send()
        .await
        .unwrap_err();
    assert!(error.is_timeout());
    let error = network_error(error);
    let diagnostic = error.downcast_ref::<Transient>().unwrap().message.clone();
    assert!(diagnostic.contains("127.0.0.1"), "{diagnostic}");
    assert!(
        diagnostic.contains("Tiempo de espera agotado"),
        "{diagnostic}"
    );
    for secret in [
        "private-user",
        "private-password",
        "private-path",
        "private-token",
        "private-cookie",
    ] {
        assert!(!diagnostic.contains(secret), "{diagnostic}");
    }
}

#[tokio::test]
async fn failed_tls_connection_identifies_the_host_instead_of_an_http_status() {
    use std::io::Write;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
            .unwrap();
    });
    let error = reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(Duration::from_secs(3))
        .build()
        .unwrap()
        .get(format!("https://{address}/secret-url"))
        .send()
        .await
        .unwrap_err();
    server.join().unwrap();
    assert!(error.is_connect());
    assert!(!error.is_timeout());
    let diagnostic = network_error(error).to_string();
    assert!(
        diagnostic.contains("No se pudo conectar con 127.0.0.1"),
        "{diagnostic}"
    );
    assert!(!diagnostic.contains("secret-url"));
    assert!(!diagnostic.contains("HTTP"));
}
