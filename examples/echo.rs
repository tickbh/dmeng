use async_trait::async_trait;
use std::{env, error::Error, time::Duration};

use tokio::net::TcpListener;
use webparse::Response;
use wmhttp::{self, Body, HttpTrait, Middleware, ProtResult, RecvRequest, RecvResponse, Server};

// #[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

struct Operate;

#[async_trait]
impl HttpTrait for Operate {
    async fn operate(&mut self, req: RecvRequest) -> ProtResult<RecvResponse> {
        tokio::time::sleep(Duration::new(1, 1)).await;
        let response = Response::builder()
            .version(req.version().clone())
            .body("Hello World\r\n".to_string())?;
        Ok(response.into_type())
    }
}

struct HelloMiddleware;
#[async_trait]
impl Middleware for HelloMiddleware {
    async fn process_request(
        &mut self,
        request: &mut RecvRequest,
    ) -> ProtResult<Option<RecvResponse>> {
        println!("hello request {}", request.url());
        Ok(None)
    }

    async fn process_response(&mut self, response: &mut RecvResponse) -> ProtResult<()> {
        println!("hello response {}", response.status());
        Ok(())
    }
}

async fn run_main() -> Result<(), Box<dyn Error>> {
    // 在main函数最开头调用这个方法
    let _file_name = format!("heap-{}.json", std::process::id());
    // let _profiler = dhat::Profiler::builder().file_name(file_name).build();
    //
    // let _profiler = dhat::Profiler::new_heap();

    let res = Response::text().body("").unwrap();
    println!("res size = {:?}", std::mem::size_of_val(&res));

    let rec = Body::empty();
    rec.print_debug();
    println!("rec size = {:?}", std::mem::size_of_val(&rec));

    // env_logger::init();
    // console_subscriber::init();
    let addr = env::args()
        .nth(1)
        .unwrap_or_else(|| "0.0.0.0:8080".to_string());
    let server = TcpListener::bind(&addr).await?;
    println!("Listening on: {}", addr);
    loop {
        let (stream, addr) = server.accept().await?;
        tokio::spawn(async move {
            // let (sender, _receiver) = channel(10);
            // let control = Control::new(
            //     ControlConfig {
            //         next_stream_id: 1.into(),
            //         // Server does not need to locally initiate any streams
            //         initial_max_send_streams: 0,
            //         max_send_buffer_size: 0,
            //         reset_stream_duration: Duration::from_millis(1),
            //         reset_stream_max: 0,
            //         remote_reset_stream_max: 0,
            //         settings: Settings::ack(),
            //     },
            //     sender,
            //     false,
            // );
            // let s = std::mem::size_of_val(&control);
            let recv = Body::empty();
            println!("recv = {:?}", std::mem::size_of_val(&recv));
            recv.print_debug();
            let x = vec![0; 1900];
            // println!("size = {:?}", s);
            println!("size = {:?}", std::mem::size_of_val(&x));
            let mut server = Server::new(stream, Some(addr));
            server.middle(HelloMiddleware);
            println!("server size size = {:?}", std::mem::size_of_val(&server));
            // println!("size = {:?}", data_size(&server));
            server.set_callback_http(Box::new(Operate));
            let e = server.incoming().await;
            println!("close server ==== addr = {:?} e = {:?}", addr, e);
        });
    }
}

#[tokio::main]
async fn main() {
    env_logger::init();
    if let Err(e) = run_main().await {
        println!("运行wmproxy发生错误:{:?}", e);
    }
}
