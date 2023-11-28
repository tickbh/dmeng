use std::{env, error::Error, time::Duration};
use async_trait::async_trait;

use tokio::{net::TcpListener};
use webparse::{Response};
use wenmeng::{self, ProtResult, Server, RecvRequest, RecvResponse, OperateTrait, Middleware};

struct Operate;

#[async_trait]
impl OperateTrait for Operate {
    async fn operate(&mut self, req: &mut RecvRequest) -> ProtResult<RecvResponse> {
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
    async fn process_request(&mut self, request: &mut RecvRequest) -> ProtResult<Option<RecvResponse>> {
        println!("hello request {}", request.url());
        Ok(None)
    }

    async fn process_response(&mut self, _request: &mut RecvRequest, response: &mut RecvResponse) -> ProtResult<()> {
        println!("hello response {}", response.status());
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    
    env_logger::init();
    let addr = env::args()
        .nth(1)
        .unwrap_or_else(|| "0.0.0.0:8080".to_string());
    let server = TcpListener::bind(&addr).await?;
    println!("Listening on: {}", addr);
    loop {
        let (stream, addr) = server.accept().await?;
        tokio::spawn(async move {
            let mut server = Server::new(stream, Some(addr));
            server.middle(HelloMiddleware);
            let operate = Operate;
            let e = server.incoming(operate).await;
            println!("close server ==== addr = {:?} e = {:?}", addr, e);
        });
    }
}
