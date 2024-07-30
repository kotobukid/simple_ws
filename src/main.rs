use redis;
use tokio;
use std::env;
use redis::{RedisResult, RedisError, Commands, Value as RedisValue, FromRedisValue};
use std::str::FromStr;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value as JsonValue};
use std::collections::HashMap;
use axum::{routing::{get, post}, Router, response::Html, Json, http::StatusCode,
           extract::{
               Form,
               ws::{WebSocketUpgrade, Message as WsMessage, WebSocket},
           }, Extension};
use tower_http::{
    services::{ServeDir},
    trace::TraceLayer,
};


use clap::{Error, Parser};
use std::net::SocketAddr;
use tokio::sync::Mutex;
use std::sync::Arc;
use redis::Connection;
use axum::response::{IntoResponse};
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use futures_util::SinkExt;
use tower::{BoxError, Service};
use tracing_subscriber::fmt;
use futures_util::stream::{SplitSink, StreamExt};
use std::sync::mpsc;
use std::sync::mpsc::{Receiver, RecvError, Sender};
use axum::extract::ws::Message;
use uuid::Uuid;

#[derive(Clone)]
struct MyService;

impl Service<u8> for MyService {
    type Response = u16;
    type Error = BoxError;
    type Future = Pin<Box<dyn std::future::Future<Output=Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: u8) -> Self::Future {
        let fut = async move {
            Ok(req as u16 * 2)
        };
        Box::pin(fut)
    }
}

type Counter = Arc<Mutex<i64>>;

#[allow(dead_code)]
async fn hello_world_handler(
    counter: Extension<Counter>,
    name: &str,
) -> Result<Html<String>, Infallible> {
    println!("name: {}", name);
    let mut count = counter.lock().await;
    *count += 1;
    let count = *count;

    Ok(Html(format!("<h1>Hello, World! inner counter is {}</h1>\
    <br />\
    <a href=\"/api/hello.json\">json sample</a>", count)))
}

#[derive(Debug, Deserialize, Serialize, Default)]
struct ClientMessage {
    body: String,
}


async fn accept_form(Form(input): Form<ClientMessage>) -> impl IntoResponse {
    dbg!(&input);
    println!("{:?}", input);
    let message = ClientMessage { body: format!("{}", input.body) };

    (StatusCode::OK, Json(message))
}

async fn accept_json(Json(input): Json<ClientMessage>) -> impl IntoResponse {
    dbg!(&input);
    let message = ClientMessage { body: format!("hello {}", input.body) };
    (StatusCode::OK, Json(message))
}

async fn handler_404() -> impl IntoResponse {
    (StatusCode::NOT_FOUND, "nothing to see here")
}

async fn serve(app: Router, port: u16, redis_con: Connection, rx: Receiver<String>) {
    let counter: Counter = Arc::new(Mutex::new(0));
    let redis: Arc<Mutex<Connection>> = Arc::new(Mutex::new(redis_con));
    let rx: Arc<Mutex<Receiver<String>>> = Arc::new(Mutex::new(rx));

    let room_map: Arc<Mutex<HashMap<Uuid, Vec<ManagedSocket>>>> = Arc::new(Mutex::new(HashMap::new())) ;

    let layer: Extension<Counter> = Extension(counter);
    let layer_redis: Extension<Arc<Mutex<Connection>>> = Extension(redis);
    let layer_receiver: Extension<Arc<Mutex<Receiver<String>>>> = Extension(rx);
    let layer_room_map : Extension<Arc<Mutex<HashMap<Uuid, Vec<ManagedSocket>>>>> = Extension(room_map);

    let app = app.layer(layer)
        .layer(layer_redis)
        .layer(layer_receiver)
        .layer(layer_room_map)
        .fallback(handler_404);

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    tracing::debug!("listening on {}", listener.local_addr().unwrap());
    axum::serve(listener, app.layer(TraceLayer::new_for_http()))
        .await
        .unwrap();
}


#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[arg(short, long, default_value_t = 3000)]
    port: u16,
}

fn using_serve_dir() -> Router {
    Router::new()
        .nest("/api", Router::new()
            .route("/hello_ajax.json", post(accept_json))
            .route("/ws", get(websocket_handler)),
        )

        .nest_service("/", ServeDir::new("out"))
}

async fn websocket_handler(ws: WebSocketUpgrade,
                           Extension(redis_con): Extension<Arc<Mutex<Connection>>>,
                           Extension(rx): Extension<Arc<Mutex<Receiver<String>>>>,
                           Extension(room_map): Extension<Arc<Mutex<HashMap<Uuid, Vec<ManagedSocket>>>>>
) -> impl IntoResponse {
    ws.on_upgrade(|socket| bind_websocket_events(socket, redis_con, rx, room_map))
}

struct ManagedSocket {
    uuid: Uuid,
    // socket: Arc<Mutex<SplitSink<WebSocketStream<TcpStream>, Message>>>,
    socket: Arc<Mutex<SplitSink<WebSocket, Message>>>,
}

async fn bind_websocket_events(
    socket: WebSocket,
    redis_con: Arc<Mutex<Connection>>,
    rx: Arc<Mutex<Receiver<String>>>,
    room_map: Arc<Mutex<HashMap<Uuid, Vec<ManagedSocket>>>>
) {
    // let (mut sender, mut receiver) = socket.split();
    // let sender = Arc::new(Mutex::new(sender));

    let id = Uuid::new_v4();

    let (sender, mut receiver) = socket.split();
    let sender = Arc::new(Mutex::new(sender));

    let ms = ManagedSocket {
        uuid: id,
        socket: sender,
    };

    let mut rm = room_map.lock().await;
    let room_id: Uuid = Uuid::new_v4();
    let sockets = rm.get_mut(&room_id);
    match sockets {
        Some(sockets) => {
            sockets.push(ms);
        },
        None => {
            rm.insert(room_id, vec!(ms));
        }
    }

    println!("new connection #{}", ms.uuid);

    while let Some(message) = receiver.next().await {
        let mut locked_sender = ms.socket.lock().await;
        match message {
            Ok(msg) => {
                // Simply send the received message back.
                println!("message sent to socket #{}", ms.uuid);

                if let Err(err) = locked_sender.send(msg).await {
                    eprintln!("Error sending WebSocket message: {}", err);
                }
            }
            Err(err) => eprintln!("Error receiving WebSocket message: {}", err),
        }
    }

    //
    // let closed = Arc::new(AtomicBool::new(false));
    //
    // let ws_task = {
    //     let closed = Arc::clone(&closed);
    //
    //     tokio::spawn(async move {
    //         while let Some(message) = receiver.next().await {
    //             match message {
    //                 Ok(WsMessage::Text(payload)) => {
    //                     let key = &payload.clone();
    //                     let mut con = redis_con.lock().await;
    //                     redis::cmd("SET").arg(&key).arg(&payload).query::<()>(&mut con).unwrap();
    //                     let _: () = con.publish("publish_message", &key).unwrap();
    //                 }
    //                 Ok(WsMessage::Close(_)) => {
    //                     closed.store(true, Ordering::Relaxed);
    //                     break;
    //                 }
    //                 Err(err) => eprintln!("Error receiving WebSocket message: {}", err), // Handle errors
    //                 _ => continue,
    //             }
    //         }
    //     })
    // };
    //
    // let rx_task = {
    //     let closed = Arc::clone(&closed);
    //
    //     tokio::spawn(async move {
    //         while let Ok(message) = rx.lock().await.recv() {
    //             if closed.load(Ordering::Relaxed) {
    //                 break;
    //             }
    //
    //             match sender.lock().await.send(WsMessage::Text(message)).await {
    //                 Ok(_) => println!("send success"),
    //                 Err(err) => eprintln!("Error sending WebSocket message via receiver: {}", err), // Handle errors
    //             }
    //         }
    //     })
    // };
    //
    // tokio::join!(ws_task, rx_task);
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    dotenv::dotenv().ok();

    let args = Args::parse();

    let host = env::var("REDIS_HOST").unwrap();

    let client = redis::Client::open(host).expect("Redis connect failed(1)");
    let con = client.get_connection().expect("Redis connect failed(2)");

    let (tx, rx) = mpsc::channel();

    fmt::Subscriber::builder()
        .with_max_level(tracing::Level::TRACE)
        .init();

    let axum_handle = serve(using_serve_dir(), args.port, con, rx);

    tokio::join!(
        axum_handle,
    );

    Ok::<(), Error>(())
}