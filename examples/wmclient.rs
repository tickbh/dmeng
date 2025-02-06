use webparse::Request;
use wmhttp::{Client, ProtResult};

async fn test_http2() -> ProtResult<()> {
    let url = "http://nghttp2.org/";

    // let url = "http://localhost:8080/";
    let req = Request::builder().method("GET").url(url).body("").unwrap();

    println!("url = {:?}", req.get_connect_url());
    let client = Client::builder().
        // http2(false)
        url(url)?.
        http2_only(true)
        .connect().await.unwrap();

    let (mut recv, _sender) = client.send2(req.into_type()).await?;
    let mut res = recv.recv().await.unwrap()?;
    res.body_mut().wait_all().await;
    println!("res = {}", res);

    // let req = Request::builder()
    //     .method("GET")
    //     .url(url.to_owned() + "blog/")
    //     .body("")
    //     .unwrap();
    // sender.send(req.into_type()).await?;
    // let res = recv.recv().await.unwrap();
    println!("res = {}", res);
    Ok(())
}

#[allow(dead_code)]
async fn test_https2() -> ProtResult<()> {
    // let req = Request::builder().method("GET").url("http://nghttp2.org/").upgrade_http2(settings).body("").unwrap();
    let req = Request::builder()
        .method("GET")
        .url("https://nghttp2.org/")
        // .header("accept", "*/*")
        .body("")
        .unwrap();

    // let req = Request::builder().method("GET").url("http://www.baidu.com/").upgrade_http2().body("").unwrap();
    println!("req = {}", req);
    let client = Client::builder()
        // .http2_only(true)
        .url(req.url().clone())?
        .connect()
        .await
        .unwrap();

    let mut recv = client.send(req.into_type()).await.unwrap();
    while let Some(res) = recv.recv().await {
        let mut res = res?;
        res.body_mut().wait_all().await;
        println!("res = {}", res);
    }

    Ok(())
    // println!("res = {:?}", res);
}

#[tokio::main]
async fn main() {
    let _ = test_http2().await;
    // test_https2().await;
    return;
}
