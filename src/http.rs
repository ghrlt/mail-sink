use std::collections::HashMap;
use sled::Db;
use std::error::Error;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::TcpStream;


use tokio::sync::{Mutex as AsyncMutex, Mutex};

use crate::smtp::Mail;
use url::form_urlencoded;
use url::Url;

#[derive(Debug, PartialEq, Eq, Hash)]
enum Method {
    GET,
    POST,
    PUT,
    DELETE,
    // Add other methods as needed
}

impl Method {
    fn from_str(method: &str) -> Option<Method> {
        match method.to_uppercase().as_str() {
            "GET" => Some(Method::GET),
            "POST" => Some(Method::POST),
            "PUT" => Some(Method::PUT),
            "DELETE" => Some(Method::DELETE),
            _ => None,
        }
    }
}

// Define a type alias for the handler function
type Handler = Box<
    dyn Fn(
        Request,
        Arc<AsyncMutex<BufWriter<tokio::net::tcp::OwnedWriteHalf>>>,
        Arc<Mutex<Db>>,
    ) -> Pin<Box<dyn Future<Output = Result<(), Box<dyn Error + Send + Sync>>> + Send>>
    + Send
    + Sync,
>;

// Define a simple Request struct
#[allow(dead_code)] // will be used later
struct Request {
    method: Method,
    path: String,
    query: HashMap<String, String>,
    params: HashMap<String, String>,
}

pub(crate) async fn handle_client(
    stream: TcpStream,
    db: Arc<Mutex<Db>>,
    key: &str,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let (reader, writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let writer = Arc::new(AsyncMutex::new(BufWriter::new(writer)));

    // Read the request line
    let mut request_line = String::new();
    let bytes_read = reader.read_line(&mut request_line).await?;
    if bytes_read == 0 {
        return Ok(());
    }

    // Parse the request line
    let request_line = request_line.trim_end();
    let mut parts = request_line.split_whitespace();
    let method_str = parts.next();
    let path_and_query = parts.next();
    let _version = parts.next();

    if let (Some(method_str), Some(path_and_query)) = (method_str, path_and_query) {
        // parse the method
        let method = Method::from_str(method_str);
        if method.is_none() {
            // Method isn't Allowed
            writer.lock().await.get_mut().shutdown().await?;
            return Ok(());
        }
        let method = method.unwrap();

        // parse the URL to handle path and query parameters
        let url = Url::parse(&format!("http://localhost{}", path_and_query))?;
        let path = url.path().to_string();
        let query_pairs = form_urlencoded::parse(url.query().unwrap_or("").as_bytes())
            .into_owned()
            .collect::<HashMap<String, String>>();

        // check if the key is provided and valid before proceeding
        if let Some(k) = query_pairs.get("k") {
            if k != key {
                // 403 Forbidden, just close the connection without any response to avoid leaking information
                writer.lock().await.get_mut().shutdown().await?;
                return Ok(());
            }
        } else {
            // 401 Unauthorized, just close the connection without any response to avoid leaking information
            writer.lock().await.get_mut().shutdown().await?;
            return Ok(());
        }

        let routes = build_routes();
        if let Some((handler, params)) = find_handler(&routes, &method, &path) {
            let request = Request {
                method,
                path,
                query: query_pairs,
                params,
            };
            handler(request, writer.clone(), db.clone()).await?;
        } else {
            let mut writer = writer.lock().await;
            writer.write_all(b"HTTP/1.1 404 Not Found\r\n\r\n").await?;
            writer.flush().await?;
        }
    } else {
        // bad request (most likely a skill issue)
        let mut writer = writer.lock().await;
        writer.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n").await?;
        writer.flush().await?;
    }

    Ok(())
}

// function to build the routing table
fn build_routes() -> Vec<(Method, String, Handler)> {
    vec![
        (
            Method::GET,
            "/mails/:mail_id".to_string(),
            Box::new(|request, writer, db| Box::pin(get_mail_handler(request, writer, db))),
        ),
        (
            Method::DELETE,
            "/mails/:mail_id".to_string(),
            Box::new(|request, writer, db| Box::pin(delete_mail_handler(request, writer, db))),
        ),
        (
            Method::GET,
            "/mails".to_string(),
            Box::new(|request, writer, db| Box::pin(get_mails_handler(request, writer, db))),
        ),
        // TODO: Add more routes here:
        //    - DELETE /mails           (delete all stored emails)
        //    - GET /preview/<mail_id>  (visual preview of the email)
        //    - GET /panel              (admin panel to view all emails and delete them)
        //    - POST /info              (basic info about the server, mail count, etc.)
    ]
}

// function to find the appropriate handler
fn find_handler<'a>(
    routes: &'a [(Method, String, Handler)],
    method: &Method,
    request_path: &str,
) -> Option<(&'a Handler, HashMap<String, String>)> {
    for (route_method, route_path, handler) in routes {
        if method == route_method {
            if let Some(params) = match_path(route_path, request_path) {
                return Some((handler, params));
            }
        }
    }
    None
}

// function to match paths with parameters
fn match_path(route_path: &str, request_path: &str) -> Option<HashMap<String, String>> {
    let route_parts: Vec<&str> = route_path.trim_end_matches('/').split('/').collect();
    let request_parts: Vec<&str> = request_path.trim_end_matches('/').split('/').collect();

    if route_parts.len() != request_parts.len() {
        return None;
    }

    let mut params = HashMap::new();

    for (route_part, request_part) in route_parts.iter().zip(request_parts.iter()) {
        if route_part.starts_with(':') {
            let name = route_part.trim_start_matches(':');
            params.insert(name.to_string(), request_part.to_string());
        } else if route_part != request_part {
            return None;
        }
    }

    Some(params)
}

//     HANDLERS     //

async fn get_mail_handler(
    request: Request,
    writer: Arc<AsyncMutex<BufWriter<tokio::net::tcp::OwnedWriteHalf>>>,
    db: Arc<Mutex<Db>>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    println!("GET /mails/:mail_id");
    let mail_id = request.params.get("mail_id").unwrap();
    println!("Mail ID: {}", mail_id);

    let db = db.lock().await;
    let result = db.get(mail_id.as_bytes());

    let mut writer = writer.lock().await;

    if let Ok(Some(data)) = result {
        println!("Mail found!");
        let mail: Mail = bincode::deserialize(&data)?;
        let json = serde_json::to_string(&mail)?;

        writer.write_all(b"HTTP/1.1 200 OK\r\n").await?;
        writer.write_all(b"Content-Type: application/json\r\n").await?;
        writer
            .write_all(format!("Content-Length: {}\r\n", json.len()).as_bytes())
            .await?;
        writer.write_all(b"\r\n").await?;
        writer.write_all(json.as_bytes()).await?;
    } else {
        writer.write_all(b"HTTP/1.1 404 Not Found\r\n\r\n").await?;
    }

    writer.flush().await?;
    Ok(())
}

async fn delete_mail_handler(
    request: Request,
    writer: Arc<AsyncMutex<BufWriter<tokio::net::tcp::OwnedWriteHalf>>>,
    db: Arc<Mutex<Db>>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mail_id = request.params.get("mail_id").unwrap();

    let db = db.lock().await;
    let result = db.remove(mail_id.as_bytes());

    let mut writer = writer.lock().await;

    if result.is_ok() {
        writer.write_all(b"HTTP/1.1 200 OK\r\n\r\n").await?;
    } else {
        writer.write_all(b"HTTP/1.1 404 Not Found\r\n\r\n").await?;
    }
    writer.flush().await?;
    Ok(())
}

async fn get_mails_handler(
    request: Request,
    writer: Arc<AsyncMutex<BufWriter<tokio::net::tcp::OwnedWriteHalf>>>,
    db: Arc<Mutex<Db>>
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let limit = request.query.get("limit").unwrap_or(&String::from("10")).parse::<usize>().unwrap();
    let offset = request.query.get("offset").unwrap_or(&String::from("0")).parse::<usize>().unwrap();

    let db = db.lock().await;
    let mut iter = db.iter();
    let mut mails = Vec::new();
    let mut count = 0;

    for _ in 0..offset {
        if iter.next().is_none() {
            break;
        }
    }

    while let Some(result) = iter.next() {
        let (_, data) = result?;
        let mail: Mail = bincode::deserialize(&data)?;
        mails.push(mail);
        count += 1;
        if count >= limit {
            break;
        }
    }

    let json = serde_json::to_string(&mails)?;

    let mut writer = writer.lock().await;
    writer.write_all(b"HTTP/1.1 200 OK\r\n").await?;
    writer.write_all(b"Content-Type: application/json\r\n").await?;
    writer.write_all(format!("Content-Length: {}\r\n", json.len()).as_bytes()).await?;
    writer.write_all(b"\r\n").await?;
    writer.write_all(json.as_bytes()).await?;

    writer.flush().await?;
    Ok(())
}
