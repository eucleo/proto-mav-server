use futures::stream::{Stream, StreamExt};
use futures::{Future, Sink, SinkExt};
use hyper::service::{make_service_fn, service_fn};
use hyper::{
    header, server::conn::AddrStream, upgrade, Body, Request, Response, Server, StatusCode,
};
use log::{error, info, Level, LevelFilter, Metadata, Record};
use proto_mav::proto::common::*;
use std::convert::Infallible;
use std::net::SocketAddr;
use tokio_tungstenite::WebSocketStream;
use tungstenite::{handshake, Error};

// Our own simple logger, should replace with something better.
struct SimpleLogger;

impl log::Log for SimpleLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= Level::Info
    }

    fn log(&self, record: &Record) {
        if self.enabled(record.metadata()) {
            println!(
                "{}: {} - {}",
                record.level(),
                record.target(),
                record.args()
            );
        }
    }

    fn flush(&self) {}
}

static LOGGER: SimpleLogger = SimpleLogger;

async fn ws_heartbeat(
    _request: Request<Body>,
    remote_addr: SocketAddr,
    mut stream: WebSocketStream<upgrade::Upgraded>,
) {
    info!("heartbeat request from {}", remote_addr);
    let hb = Heartbeat {
        custom_mode: 0,
        r#type: MavType::Quadrotor as i32,
        autopilot: MavAutopilot::Ardupilotmega as i32,
        base_mode: MavModeFlag::Undefined as u32,
        system_status: MavState::Standby as i32,
        mavlink_version: 0x3,
    };
    //let mut times = 0;
    loop {
        let ser = serde_json::to_string(&hb).unwrap();
        match stream.send(tungstenite::Message::Text(ser)).await {
            Ok(_) => {
                info!("sent heartbeat to {}", remote_addr);
            }
            Err(e) => {
                error!(
                    "error writing heartbeat to sream on \
                                                        connection from address {}. \
                                                        Error is {}",
                    remote_addr, e
                );
                break;
            }
        }
        /*times += 1;
        if times > 4 {
            stream
                .send(tungstenite::Message::Close(None))
                .await
                .unwrap();
            info!("done sending heartbeats to {}", remote_addr);
            break;
        }*/
        tokio::time::sleep(tokio::time::Duration::from_millis(3000)).await;
    }
}

async fn ws_echo<S, I>(_request: Request<Body>, remote_addr: SocketAddr, stream: S)
where
    S: Sink<I, Error = Error> + Stream<Item = Result<I, <S as futures::Sink<I>>::Error>>,
{
    //we can split the stream into a sink and a stream
    let (ws_write, ws_read) = stream.split();

    //forward the stream to the sink to achieve echo
    match ws_read.forward(ws_write).await {
        Ok(_) => {}
        Err(Error::ConnectionClosed) => {
            info!("Connection closed normally")
        }
        Err(e) => {
            error!(
                "error creating echo stream on \
                                                    connection from address {}. \
                                                    Error is {}",
                remote_addr, e
            );
        }
    };
}

/// Upgrade a hyper http connection to a websocket connection and then call f with
/// the stream.
async fn do_websocket<F, Fut>(
    mut request: Request<Body>,
    remote_addr: SocketAddr,
    f: F,
) -> Result<Response<Body>, Infallible>
where
    Fut: Future + Send,
    F: Fn(Request<Body>, SocketAddr, WebSocketStream<upgrade::Upgraded>) -> Fut
        + Send
        + Sync
        + 'static,
{
    //assume request is a handshake, so create the handshake response
    let response = match handshake::server::create_response_with_body(&request, Body::empty) {
        Ok(response) => {
            //in case the handshake response creation succeeds,
            //spawn a task to handle the websocket connection
            tokio::spawn(async move {
                //using the hyper feature of upgrading a connection
                match upgrade::on(&mut request).await {
                    //if successfully upgraded
                    Ok(upgraded) => {
                        //create a websocket stream from the upgraded object
                        let ws_stream = WebSocketStream::from_raw_socket(
                            //pass the upgraded object
                            //as the base layer stream of the Websocket
                            upgraded,
                            tokio_tungstenite::tungstenite::protocol::Role::Server,
                            None,
                        )
                        .await;
                        f(request, remote_addr, ws_stream).await;
                    }
                    Err(e) => error!(
                        "error when trying to upgrade connection \
                                        from address {} to websocket connection. \
                                        Error is: {}",
                        remote_addr, e
                    ),
                }
            });
            //return the response to the handshake request
            response
        }
        Err(error) => {
            //probably the handshake request is not up to spec for websocket
            error!(
                "Failed to create websocket response \
                                to request from address {}. \
                                Error is: {}",
                remote_addr, error
            );
            let mut res =
                Response::new(Body::from(format!("Failed to create websocket: {}", error)));
            *res.status_mut() = StatusCode::BAD_REQUEST;
            return Ok(res);
        }
    };

    Ok::<_, Infallible>(response)
}

async fn dispatch_request(
    request: Request<Body>,
    remote_addr: SocketAddr,
) -> Result<Response<Body>, Infallible> {
    match (
        request.uri().path(),
        request.headers().contains_key(header::UPGRADE),
    ) {
        //if the request is ws_echo and the request headers contains an Upgrade key
        ("/ws_echo", true) => do_websocket(request, remote_addr, ws_echo).await,
        ("/ws_hb", true) => do_websocket(request, remote_addr, ws_heartbeat).await,
        ("/ws_echo", false) => {
            //handle the case where the url is /ws_echo, but does not have an Upgrade field
            Ok(Response::new(Body::from(
                "/ws_echo needs a websocket.\n".to_string(),
            )))
        }
        ("/ws_hb", false) => {
            //handle the case where the url is /ws_echo, but does not have an Upgrade field
            Ok(Response::new(Body::from(
                "/ws_hb needs a websocket.\n".to_string(),
            )))
        }
        (url, _) => {
            //handle any other url without an Upgrade header field
            let mut res = Response::new(Body::from(format!("{} not found.\n", url)));
            *res.status_mut() = StatusCode::NOT_FOUND;
            Ok(res)
        }
    }
}

//#[tokio::main(flavor = "current_thread")]
#[tokio::main]
async fn main() {
    log::set_logger(&LOGGER)
        .map(|()| log::set_max_level(LevelFilter::Info))
        .unwrap();

    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));

    info!("Listening on {} for http or websocket connections.", addr);

    // A `Service` is needed for every connection, so this
    // creates one from our `handle_request` function.
    let make_svc = make_service_fn(|conn: &AddrStream| {
        let remote_addr = conn.remote_addr();
        async move {
            // service_fn converts our function into a `Service`
            Ok::<_, Infallible>(service_fn(move |request: Request<Body>| {
                dispatch_request(request, remote_addr)
            }))
        }
    });

    let server = Server::bind(&addr).serve(make_svc);

    // Run this server for... forever!
    if let Err(e) = server.await {
        error!("server error: {}", e);
    }
}
