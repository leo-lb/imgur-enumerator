use clap::{App, Arg};
use futures::{stream, Future, Stream};
use hyper::{Body, Client, Request, StatusCode, Uri, header};
use hyper_tls::HttpsConnector;
use rand::distributions::Alphanumeric;
use rand::{thread_rng, Rng};
use std::fs::OpenOptions;
use std::io::prelude::*;
use std::iter;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tokio;

const BASE_URL: &str = "https://i.imgur.com/";

struct UriGenerator {
    base_url: String,
    extension: String,
}

impl UriGenerator {
    fn new(base_url: String, extension: String) -> Self {
        Self {
            base_url,
            extension,
        }
    }
}

impl Iterator for UriGenerator {
    type Item = Uri;

    fn next(&mut self) -> Option<Uri> {
        Some(
            format!(
                "{}{}{}",
                self.base_url,
                iter::repeat(())
                    .map(|()| thread_rng().sample(Alphanumeric))
                    .take(7)
                    .collect::<String>(),
                self.extension
            )
                .parse()
                .unwrap(),
        )
    }
}

fn stream_to_telegram(channel: String, token: String, rx: mpsc::Receiver<String>) {
    let https = HttpsConnector::new(4).expect("TLS initialization failed");
    let client = Client::builder().build::<_, hyper::Body>(https);

    let mut runtime = tokio::runtime::Runtime::new().expect("Unable to create a runtime");

    for image_url in rx {
        let work = client
            .get(
                format!(
                    "https://api.telegram.org/bot{}/sendPhoto?chat_id={}&photo={}",
                    token, channel, image_url
                )
                    .parse()
                    .unwrap(),
            )
            .and_then(|res| res.into_body().concat2())
            .map(|_body| {})
            .map_err(|_| {});

        runtime.block_on(work).is_err();
    }
}

fn stream_to_webhook(id: u64, token: String, rx: mpsc::Receiver<String>) {
    use serenity::http;
    use serenity::model::channel::Embed;

    let webhook = http::get_webhook_with_token(id, &token).expect("valid webhook");

    for image_url in rx {
        let resources = Embed::fake(|e| e.image(image_url));
        let _ = webhook.execute(false, |w| w.embeds(vec![resources]));
    }
}

fn stream_to_file(path: String, rx: mpsc::Receiver<String>, rx_size: Option<mpsc::Receiver<u64>>) {
    let mut file = OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .unwrap();

    if let Some(rx_size) = rx_size {
        for image_url in rx {
            if let Ok(size) = rx_size.try_recv() {
                file.write_all(format!("{} {}\n", image_url, size).as_bytes())
                    .unwrap();
            }
        }
    } else {
        for image_url in rx {
            file.write_all(format!("{}\n", image_url).as_bytes())
                .unwrap();
        }
    }
}

fn print_statistics(
    found_per_minute: Arc<AtomicUsize>,
    request_per_second: Arc<AtomicUsize>,
    total_requests: Arc<AtomicUsize>,
    total_found: Arc<AtomicUsize>,
) {
    let mut elapsed_seconds = 0;
    let mut elapsed_milliseconds = 0;

    let mut cached_found_per_seconds = 0;
    let mut cached_found_per_minute = 0;
    loop {
        print!(
            "{} req / sec - {} found / min - uptime {}s - total reqs {} - total found {}\r",
            cached_found_per_seconds,
            cached_found_per_minute,
            elapsed_seconds,
            total_requests.load(Ordering::SeqCst),
            total_found.load(Ordering::SeqCst)
        );

        std::io::stdout().flush().unwrap();

        if elapsed_milliseconds % 1000 == 0 {
            elapsed_seconds += 1;

            if elapsed_seconds % 60 == 0 {
                cached_found_per_minute = found_per_minute.load(Ordering::SeqCst);
                found_per_minute.store(0, Ordering::SeqCst);
            }

            cached_found_per_seconds = request_per_second.load(Ordering::SeqCst);
            request_per_second.store(0, Ordering::SeqCst);
        }

        thread::sleep(Duration::from_millis(50));
        elapsed_milliseconds += 50;
    }
}

fn main() {
    let matches = App::new("imgur-enumerator")
        .version("0.1")
        .arg(
            Arg::with_name("concurrent")
                .long("concurrent")
                .short("c")
                .takes_value(true)
                .default_value("4")
                .help("Maximum amount of concurrent requests at a time"),
        )
        .arg(
            Arg::with_name("webhook_id")
                .long("id")
                .short("i")
                .takes_value(true)
                .help("Discord Webhook ID"),
        )
        .arg(
            Arg::with_name("webhook_token")
                .long("token")
                .short("t")
                .takes_value(true)
                .help("Discord Webhook Token"),
        )
        .arg(
            Arg::with_name("tg_channel")
                .long("tg-channel")
                .short("k")
                .takes_value(true)
                .number_of_values(1)
                .allow_hyphen_values(true)
                .help("Telegram Channel ID"),
        )
        .arg(
            Arg::with_name("tg_token")
                .long("tg-token")
                .short("l")
                .takes_value(true)
                .help("Telegram Bot Token"),
        )
        .arg(
            Arg::with_name("export_file")
                .long("export")
                .short("e")
                .takes_value(true)
                .help("Path to file where found links will be written"),
        )
        .arg(
            Arg::with_name("user_agent")
                .long("user-agent")
                .short("u")
                .takes_value(true)
                .default_value("Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:65.0) Gecko/20100101 Firefox/65.0")
                .help("Value of User-Agent header which will be used in all requests")
        )
        .arg(
            Arg::with_name("report_size")
                .long("report-size")
                .short("s")
                .takes_value(false)
                .help("Report the image size when exporting to a file")
        )
        .get_matches();

    let n_concurrent = matches.value_of("concurrent").unwrap().parse().unwrap();
    let user_agent = matches.value_of("user_agent").unwrap().to_string();

    let (tx, rx) = mpsc::channel::<String>();
    let (tx_hook, rx_hook) = mpsc::channel::<String>();
    let (tx_tg, rx_tg) = mpsc::channel::<String>();
    let (tx_size, rx_size) = mpsc::channel::<u64>();

    if matches.is_present("webhook_id") && matches.is_present("webhook_token") {
        let id = matches.value_of("webhook_id").unwrap().parse().unwrap();
        let token: String = matches.value_of("webhook_token").unwrap().to_string();

        thread::spawn(move || stream_to_webhook(id, token, rx_hook));
    }

    if matches.is_present("export_file") {
        let export_path: String = matches.value_of("export_file").unwrap().to_string();

        if matches.is_present("report_size") {
            thread::spawn(move || stream_to_file(export_path, rx, Some(rx_size)));
        } else {
            thread::spawn(move || stream_to_file(export_path, rx, None));
        }
    }

    if matches.is_present("tg_channel") && matches.is_present("tg_token") {
        let channel = matches.value_of("tg_channel").unwrap().to_string();
        let token: String = matches.value_of("tg_token").unwrap().to_string();

        thread::spawn(move || stream_to_telegram(channel, token, rx_tg));
    }

    let request_per_second = Arc::new(AtomicUsize::new(0));
    let found_per_minute = Arc::new(AtomicUsize::new(0));

    let total_requests = Arc::new(AtomicUsize::new(0));
    let total_found = Arc::new(AtomicUsize::new(0));

    {
        let found_per_minute = found_per_minute.clone();
        let request_per_second = request_per_second.clone();

        let total_requests = total_requests.clone();
        let total_found = total_found.clone();

        thread::spawn(move || {
            print_statistics(
                found_per_minute,
                request_per_second,
                total_requests,
                total_found,
            )
        });
    }

    println!("Starting with {} concurrent requests.", n_concurrent);

    loop {
        let request_per_second = request_per_second.clone();
        let found_per_minute = found_per_minute.clone();

        let total_requests = total_requests.clone();
        let total_found = total_found.clone();

        let tx = tx.clone();
        let tx_hook = tx_hook.clone();
        let tx_tg = tx_tg.clone();
        let tx_size = tx_size.clone();

        let https = HttpsConnector::new(4).expect("TLS initialization failed");
        let client = Client::builder().build::<_, hyper::Body>(https);

        let images = UriGenerator::new(BASE_URL.to_string(), ".png".to_string());

        let user_agent = user_agent.clone();

        let work = stream::iter_ok(images)
            .map(move |uri| {
                let mut request = Request::head(uri.clone());

                request.header("User-Agent", user_agent.clone());

                client
                    .request(request.body(Body::empty()).unwrap())
                    .map(move |res| (res, uri))
            })
            .buffer_unordered(n_concurrent)
            .and_then(move |(res, uri)| {
                request_per_second.fetch_add(1, Ordering::SeqCst);
                total_requests.fetch_add(1, Ordering::SeqCst);

                if res.status() == StatusCode::OK {
                    found_per_minute.fetch_add(1, Ordering::SeqCst);
                    total_found.fetch_add(1, Ordering::SeqCst);

                    let image_url = format!(
                        "{}://{}{}",
                        uri.scheme_str().unwrap(),
                        uri.authority_part().unwrap(),
                        uri.path_and_query().unwrap()
                    );

                    println!("{}found valid image at {}", "\x1B[K", image_url);

                    if let Some(size) = res.headers().get(header::CONTENT_LENGTH) {
                        if let Ok(size) = size.to_str() {
                            if let Ok(size) = size.parse::<u64>() {
                                tx_size.send(size).is_err();
                            }
                        }
                    }
                    
                    tx.send(image_url.clone()).is_err();
                    tx_hook.send(image_url.clone()).is_err();
                    tx_tg.send(image_url.clone()).is_err();
                }
                res.into_body().concat2()
            })
            .for_each(|_body| Ok(()))
            .map_err(|e| {
                eprintln!("{}", e);
            });

        tokio::run(work);
    }
}
