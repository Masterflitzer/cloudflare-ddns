#![feature(ip)]

pub(crate) mod api;
pub(crate) mod config;
pub(crate) mod errors;
pub(crate) mod ip;
pub(crate) mod structs;

use api::{api_get, api_patch};
use clap::Parser;
use errors::{handle_errors, ErrorKind};
use ip::determine_ip;
use reqwest::{Client as HttpClient, Response, Url};
use serde::de::DeserializeOwned;
use serde_json::Value as Json;
use std::{net::IpAddr, process::exit, str::FromStr};
use structs::{
    cloudflare::request::PatchDnsRecord,
    cloudflare::response::{ListDnsRecords, ListZone},
    cloudflare::Cloudflare,
    Args,
};

const VERSION: Option<&str> = option_env!("CARGO_PKG_VERSION");

#[tokio::main]
async fn main() {
    let args = Args::parse();

    if args.version {
        println!("{}", VERSION.unwrap_or("unknown"));
        return;
    }

    let config_path = match args.config {
        Some(x) => x,
        None => match config::path() {
            Ok(x) => x,
            Err(e) => {
                handle_errors(&ErrorKind::ConfigPath(e));
                exit(100);
            }
        },
    };

    if args.configuration {
        println!("{}", config_path.display());
        return;
    }

    let config = match config::get(config_path) {
        Ok(x) => x,
        Err(e) => {
            handle_errors(&ErrorKind::Config(e));
            exit(101)
        }
    };

    let (ipv4, ipv6) = determine_ip(&config).await;

    if ipv4.is_none() {
        handle_errors(&ErrorKind::IPv4)
    };

    if ipv6.is_none() {
        handle_errors(&ErrorKind::IPv6)
    };

    if ipv4.is_none() && ipv6.is_none() {
        println!("Neither IPv4 nor IPv6 address could be determined");
        exit(102)
    }

    let http: HttpClient = HttpClient::new();

    let api_base: Url = match Url::parse("https://api.cloudflare.com/client/v4/") {
        Ok(x) => x,
        Err(e) => {
            handle_errors(&ErrorKind::Unknown(Box::new(e)));
            exit(103)
        }
    };

    let url_list_zones = match api_base.join("zones") {
        Ok(x) => x,
        Err(e) => {
            handle_errors(&ErrorKind::Unknown(Box::new(e)));
            exit(104)
        }
    };

    let response_zones = match api_get(&http, url_list_zones, &config.api_token).await {
        Ok(x) => x,
        Err(_) => {
            handle_errors(&ErrorKind::Api);
            exit(105);
        }
    };

    let json_zones = match deserialize_response(response_zones).await {
        Ok(x) => x,
        Err(e) => {
            handle_errors(&e);
            exit(106);
        }
    };

    let data_zones = match deserialize_json_value::<Vec<ListZone>>(json_zones.result).await {
        Ok(x) => x,
        Err(e) => {
            handle_errors(&e);
            exit(107);
        }
    };

    for config_zone in config.records.keys() {
        let zone = match obtain_zone(&data_zones, config_zone).await {
            Some(x) => x,
            None => {
                println!(
                    "Skipping \"{}\" because the corresponding zone could not be found",
                    &config_zone
                );
                continue;
            }
        };

        let url_list_dns_records =
            match api_base.join(format!("zones/{}/dns_records", zone.id).as_str()) {
                Ok(x) => x,
                Err(e) => {
                    handle_errors(&ErrorKind::Unknown(Box::new(e)));
                    exit(108)
                }
            };

        let response_records = match api_get(&http, url_list_dns_records, &config.api_token).await {
            Ok(x) => x,
            Err(_) => {
                handle_errors(&ErrorKind::Api);
                exit(109);
            }
        };

        let json_records = match deserialize_response(response_records).await {
            Ok(x) => x,
            Err(e) => {
                handle_errors(&e);
                match e {
                    ErrorKind::NoSuccessHttp | ErrorKind::NoSuccessJson => continue,
                    _ => exit(110),
                }
            }
        };

        let data_records =
            match deserialize_json_value::<Vec<ListDnsRecords>>(json_records.result).await {
                Ok(x) => x,
                Err(e) => {
                    handle_errors(&e);
                    exit(111);
                }
            };

        let config_records = match config.records.get(config_zone) {
            Some(x) => x,
            None => continue,
        };

        for config_record in config_records {
            let record_name = match config_record == "@" {
                true => config_zone.to_owned(),
                false => format!("{}.{}", config_record, config_zone),
            };

            let records = obtain_records(&data_records, record_name.as_str()).await;

            if records.is_empty() {
                println!(
                    "Skipping \"{}\" because the corresponding records could not be found",
                    &config_record
                );
                continue;
            }

            'outer: for record in records {
                let url_patch_dns_records = match api_base
                    .join(format!("zones/{}/dns_records/{}", zone.id, record.id).as_str())
                {
                    Ok(x) => x,
                    Err(e) => {
                        handle_errors(&ErrorKind::Unknown(Box::new(e)));
                        exit(112)
                    }
                };

                let ip: IpAddr = match record.type_.to_uppercase().as_str() {
                    "A" => 'inner: {
                        if let Some(ip) = ipv4 {
                            break 'inner IpAddr::V4(ip);
                        }
                        continue 'outer;
                    }
                    "AAAA" => 'inner: {
                        if let Some(ip) = ipv6 {
                            break 'inner IpAddr::V6(ip);
                        }
                        continue 'outer;
                    }
                    _ => {
                        handle_errors(&ErrorKind::NonAddressRecord);
                        continue;
                    }
                };

                let msg = format!(
                    "\"{}\" Record \"{}\" in zone \"{}\" with IP address \"{}\"",
                    record.type_, record.name, zone.name, ip
                );

                if let Ok(current_ip) = IpAddr::from_str(&record.content) {
                    if current_ip == ip {
                        println!("Already up-to-date: {}", msg);
                        continue;
                    }
                }

                let payload = PatchDnsRecord {
                    comment: None,
                    content: Some(ip),
                    name: None,
                    proxied: None,
                    tags: None,
                    ttl: None,
                };

                let response_record = match api_patch(
                    &http,
                    url_patch_dns_records,
                    &config.api_token,
                    &payload,
                )
                .await
                {
                    Ok(x) => x,
                    Err(_) => {
                        handle_errors(&ErrorKind::Api);
                        exit(113);
                    }
                };

                match deserialize_response(response_record).await {
                    Ok(x) => x,
                    Err(e) => {
                        handle_errors(&e);
                        match e {
                            ErrorKind::NoSuccessHttp | ErrorKind::NoSuccessJson => continue,
                            _ => exit(114),
                        }
                    }
                };

                println!("Successfully updated: {}", msg);
            }
        }
    }
}

async fn deserialize_response(response: Response) -> Result<Cloudflare, ErrorKind> {
    if !is_http_success(&response) {
        return Err(ErrorKind::NoSuccessHttp);
    }

    let data = response
        .json::<Cloudflare>()
        .await
        .map_err(|_| ErrorKind::Json)?;

    if !data.success {
        return Err(ErrorKind::NoSuccessJson);
    }

    Ok(data)
}

async fn deserialize_json_value<T: DeserializeOwned>(data: Json) -> Result<T, ErrorKind> {
    let result = serde_json::from_value::<T>(data).map_err(|_| ErrorKind::Json)?;
    Ok(result)
}

async fn obtain_zone(data: &[ListZone], zone_name: &str) -> Option<ListZone> {
    data.iter().find(|x| x.name == zone_name).cloned()
}

async fn obtain_records(data: &[ListDnsRecords], record_name: &str) -> Vec<ListDnsRecords> {
    data.iter()
        .filter(|x| x.name == record_name)
        .filter(|x| x.type_.to_uppercase() == "A" || x.type_.to_uppercase() == "AAAA")
        .cloned()
        .collect()
}

fn is_http_success(response: &Response) -> bool {
    response.status().is_success()
}
