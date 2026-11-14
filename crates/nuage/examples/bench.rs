//! How fast a Nuage DB server answers, measured from a client on the same
//! machine (no network in the way): latency of reads and writes, one at a
//! time, then throughput with many clients at once.
//!
//! ```sh
//! cargo run --release -p nuage --example bench
//! NUAGE_BENCH_ROWS=100000 NUAGE_BENCH_CLIENTS=64 cargo run --release -p nuage --example bench
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use memoire_orm::prelude::*;
use nuage::{Encryption, Engine, EngineOptions, Server, ServerOptions};

#[derive(Model, Debug, Clone)]
#[memoire(table = "articles")]
struct Article {
    #[memoire(primary_key, auto_increment)]
    id: i64,
    #[memoire(unique)]
    slug: String,
    #[memoire(index)]
    category: i32,
    title: String,
    body: String,
    views: i64,
}

fn article(n: i64) -> Article {
    Article {
        id: 0,
        slug: format!("article-{n:07}"),
        category: (n % 50) as i32,
        title: format!("Article {n}"),
        body: "Lorem ipsum dolor sit amet. ".repeat(12),
        views: n * 7 % 1000,
    }
}

fn env(name: &str, default: usize) -> usize {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// p50, p99 and mean of microsecond samples, as `p50 0.21 ms · p99 0.48 ms`.
fn summary(mut samples: Vec<u64>) -> String {
    samples.sort_unstable();
    let at = |p: f64| samples[((samples.len() - 1) as f64 * p).round() as usize] as f64 / 1000.0;
    let mean = samples.iter().sum::<u64>() as f64 / samples.len() as f64 / 1000.0;
    format!("p50 {:.3} ms · p99 {:.3} ms · mean {mean:.3} ms", at(0.5), at(0.99))
}

async fn timed<F, Fut>(n: usize, mut f: F) -> Vec<u64>
where
    F: FnMut(usize) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let t = Instant::now();
        f(i).await;
        out.push(t.elapsed().as_micros() as u64);
    }
    out
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> memoire_orm::Result<()> {
    let rows = env("NUAGE_BENCH_ROWS", 20_000) as i64;
    let clients = env("NUAGE_BENCH_CLIENTS", 32);
    let seconds = env("NUAGE_BENCH_SECONDS", 5) as u64;
    let dir = std::env::temp_dir().join(format!("nuage-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let options = EngineOptions { encryption: Some(Encryption::from_key([1; 32])), ..EngineOptions::default() };
    let engine = Engine::open(&dir, options)?;
    let server = Server::new(engine, ServerOptions { token: Some("bench".into()), ..ServerOptions::default() });
    let running = server.listen("127.0.0.1:0").await?;
    let db = NuageDb::connect(&running.url(), "bench").await?;
    println!("Nuage DB {} — encrypted, {rows} rows, {clients} clients", nuage::VERSION);

    db.create_table::<Article>().await?;
    let t = Instant::now();
    for chunk in (0..rows).collect::<Vec<_>>().chunks(1000) {
        let batch: Vec<Article> = chunk.iter().map(|n| article(*n)).collect();
        Article::insert_many(&db, &batch).await?;
    }
    println!("load        {rows} rows in {:.2} s", t.elapsed().as_secs_f64());

    let n = 2000;
    let pk = timed(n, |i| {
        let db = db.clone();
        async move {
            Article::get(&db, (i as i64 * 7919) % rows + 1).await.unwrap();
        }
    })
    .await;
    println!("get by id   {}", summary(pk));
    let unique = timed(n, |i| {
        let db = db.clone();
        async move {
            let slug = format!("article-{:07}", (i as i64 * 104_729) % rows);
            Article::query().filter(Article::SLUG.eq(slug)).one(&db).await.unwrap();
        }
    })
    .await;
    println!("by slug     {}", summary(unique));
    let page = timed(n / 2, |i| {
        let db = db.clone();
        async move {
            Article::query()
                .filter(Article::CATEGORY.eq((i % 50) as i32))
                .order_by(Article::ID.desc())
                .limit(20)
                .all(&db)
                .await
                .unwrap();
        }
    })
    .await;
    println!("page of 20  {}", summary(page));
    let writes = timed(n / 2, |i| {
        let db = db.clone();
        async move {
            Article::update_many()
                .set(Article::VIEWS, i as i64)
                .filter(Article::ID.eq((i as i64 * 31) % rows + 1))
                .execute(&db)
                .await
                .unwrap();
        }
    })
    .await;
    println!("update      {}", summary(writes));

    // What a website sees: `POST /v1/sql` over one kept-alive connection,
    // with and without gzip.
    let stream = tokio::net::TcpStream::connect(running.local_addr()).await.map_err(|e| Error::Pool(e.to_string()))?;
    let _ = stream.set_nodelay(true);
    let (mut sender, conn) = hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
        .await
        .map_err(|e| Error::Pool(e.to_string()))?;
    tokio::spawn(conn);
    for (label, sql, gzip) in [
        ("sql by slug", "SELECT * FROM articles WHERE slug = ?", false),
        ("sql page    ", "SELECT id, slug, title FROM articles WHERE category = ? ORDER BY id DESC LIMIT 20", false),
        ("sql page gz ", "SELECT id, slug, title FROM articles WHERE category = ? ORDER BY id DESC LIMIT 20", true),
    ] {
        let mut samples = Vec::with_capacity(n);
        for i in 0..n {
            let param = if sql.contains("slug = ?") {
                serde_json::json!(format!("article-{:07}", (i as i64 * 104_729) % rows))
            } else {
                serde_json::json!((i % 50) as i32)
            };
            let body = serde_json::json!({ "sql": sql, "params": [param] }).to_string();
            let mut req = hyper::Request::post("/v1/sql")
                .header("host", "localhost")
                .header("authorization", "Bearer bench")
                .header("content-type", "application/json");
            if gzip {
                req = req.header("accept-encoding", "gzip");
            }
            let req = req.body(http_body_util::Full::new(hyper::body::Bytes::from(body))).unwrap();
            // The connection takes the next request once it is done with the last answer.
            sender.ready().await.map_err(|e| Error::Pool(e.to_string()))?;
            let t = Instant::now();
            let res = sender.send_request(req).await.map_err(|e| Error::Pool(e.to_string()))?;
            assert_eq!(res.status(), 200);
            http_body_util::BodyExt::collect(res.into_body()).await.map_err(|e| Error::Pool(e.to_string()))?;
            samples.push(t.elapsed().as_micros() as u64);
        }
        println!("{label} {}", summary(samples));
    }

    // Many clients at once: reads by id.
    let done = Arc::new(AtomicU64::new(0));
    let stop = Instant::now() + Duration::from_secs(seconds);
    let mut tasks = Vec::new();
    for c in 0..clients {
        let db = NuageDb::connect(&running.url(), "bench").await?;
        let done = done.clone();
        tasks.push(tokio::spawn(async move {
            let mut i = c as i64;
            while Instant::now() < stop {
                Article::get(&db, (i * 7919) % rows + 1).await.unwrap();
                done.fetch_add(1, Ordering::Relaxed);
                i += clients as i64;
            }
        }));
    }
    for t in tasks {
        let _ = t.await;
    }
    let total = done.load(Ordering::Relaxed);
    println!("throughput  {:.0} reads/s ({clients} clients, {seconds} s)", total as f64 / seconds as f64);

    running.shutdown().await;
    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}
