// Embedded static file serving
// TODO: Implement static file server

pub mod client_html {
    pub const CLIENT_HTML: &str = include_str!("client.html");
}

pub struct WebServer {
    // TODO: Add web server fields
}

impl WebServer {
    pub fn new() -> Self {
        Self {}
    }
}
