use actix_web::{HttpRequest, HttpResponse, Result};
use std::{ffi::OsStr, path::PathBuf};
async fn index(req: HttpRequest) -> Result<HttpResponse> {
    let path: PathBuf = req.match_info().query("filename").parse().unwrap();
    if path.file_name() == Some(OsStr::new("index.html")) {
        Ok(HttpResponse::Ok().body("body"))
    } else {
        Err(actix_web::error::ErrorNotFound("err"))
    }
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    use actix_web::{web, App, HttpServer};

    HttpServer::new(|| App::new().route("/{filename:.*}", web::get().to(index)))
        .bind(("127.0.0.1", 8080))?
        .run()
        .await
}
