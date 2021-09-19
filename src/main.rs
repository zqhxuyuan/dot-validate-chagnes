use log::*;
use tokio_tungstenite::{connect_async, tungstenite};
use url::Url;
use futures::{StreamExt, SinkExt};
use std::fs::OpenOptions;
use serde_json::Value;
use std::io::Read;
use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::time::Duration;
use std::ops::Index;
use tokio_postgres::{Client, Connection, Socket};
use tokio_postgres::tls::NoTlsStream;
use tokio_postgres::types::ToSql;

pub async fn test_substrate_validate_changes() -> Result<(), std::io::Error> {
    let case_url =
        Url::parse("ws://192.168.2.142:29955")
            .expect("Bad URL");

    let (mut ws_stream, _) = connect_async(case_url).await.map_err(|e| std::io::ErrorKind::Other)?;

    let (client, connection) =
        tokio_postgres::connect("host=192.168.2.142 port=5432 user=postgres password=postgres dbname=archive", tokio_postgres::NoTls).await.map_err(|e| std::io::ErrorKind::Other)?;
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            println!("connection error: {}", e);
        } else {
            println!("connect ok...");
        }
    });

    let prepare = client.prepare("SELECT main_changes from blocks where block_num = $1::INT4")
        .await.unwrap();

    let n1 = std::env::var("VAL_NUM_START").unwrap().parse::<i32>().unwrap();
    let n2 = std::env::var("VAL_NUM_END").unwrap().parse::<i32>().unwrap() + 1;
    println!("validate range number begin: {} {}", n1, n2);

    for number in n1..n2 as i32 {
    // for number in 7192..7193 as i32 {
        let str = r#"
        {
         "jsonrpc":"2.0",
          "id":1,
          "method":"chain_getBlockHash",
          "params": [$]
        }
        "#;
        let str = str.replace("$", &format!("{}", number));
        let msg = tungstenite::Message::Text(String::from(str));
        ws_stream.send(msg).await;

        if let Some(msg) = ws_stream.next().await {
            let msg = msg.map_err(|e| std::io::ErrorKind::Other)?;
            let hash_json: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
            let hash = &hash_json["result"];
            let hash = hash.to_string();

            let str = r#"
                {
             "jsonrpc":"2.0",
              "id":1,
              "method":"state_traceBlock",
              "params": [$, "state", ""]
            }
            "#;
            let str = str.replace("$", &hash);

            let msg = tungstenite::Message::Text(String::from(str));
            ws_stream.send(msg).await;
            if let Some(msg) = ws_stream.next().await {
                let msg = msg.map_err(|e| std::io::ErrorKind::Other)?;
                // println!("block:{} hash:{}", number, hash);
                let (ws_all_keys, append_keys, ws_buffer) = do_validate(msg.to_string());
                // println!("append keys:{:?}", append_keys);
                // println!("ws buffer:{}", ws_buffer);
                let rows = client.query(&prepare, &[&number])
                    .await.map_err(|e| std::io::ErrorKind::Other)?;
                let value: Value = rows[0].get(0);
                let map = value.as_object().unwrap();
                let mut db_buffer = String::from("{");
                map.iter().for_each(|(k, v)| {
                    if !append_keys.contains(k) && ws_all_keys.contains(k) {
                        let c = format!("\"{}\":{},", k, v.to_string());
                        db_buffer.push_str(c.as_str());
                    }
                });
                db_buffer.pop();
                db_buffer.push_str("}");
                // println!("db result:{}", db_buffer);
                if ws_buffer.as_str() == db_buffer.as_str() {
                    // println!("block number {} changes equal!", number);
                } else {
                    println!("block number {} changes NOT equal!, hash:{}", number, hash);
                    println!("  append keys:{:?}", append_keys);
                    println!("  ws_all keys:{:?}", ws_all_keys);
                    println!("  ws buffer:{}", ws_buffer);
                    println!("  db result:{}", db_buffer);
                }
            }
        }
    }

    ws_stream.close(None).await;
    // println!("validate range number end: {} {}", n1, n2);

    Ok(())
}

pub fn do_validate(buffer: String) -> (HashSet<String>, HashSet<String>, String) {
    let json: Value = serde_json::from_str(buffer.as_str()).unwrap();
    let json = &json["result"]["blockTrace"]["events"];

    let vec = json.as_array();
    let mut append_keys = HashSet::new();
    let mut ws_all_keys = HashSet::new();

    if vec.is_none() {
        return (ws_all_keys, append_keys, "".to_string());
    }
    let mut list = Vec::new();
    for ele in vec.unwrap() {
        let data = &ele["data"]["stringValues"];
        // stringValues 可能有不同的结构，比如 Put，Append 的格式都不同
        // "message": "cc59: Append 26aa394eea5630e07c48ae0c9558cef780d41e5e16056765bc8461851072c9d7=0000000000000080e36a0900000000020000"
        if data.to_string().contains("message") {
            if data.to_string().contains("Append") {
                let message = &data["message"];
                let message = message.to_string();
                let last = message.split_whitespace().last().unwrap();
                let ll: Vec<_> = last.split("=").collect();
                let key = ll.first().unwrap();
                // println!("message key:{}", key);
                append_keys.insert(format!("0x{}", key));
                continue;
            }
            continue;
        }

        let rm_get = &data["method"];
        if !rm_get.to_string().contains("Get") {
            list.push(data);
        }
    }
    list.reverse();
    let mut map = HashMap::new();
    for l in list {
        let key = l["key"].to_string();
        let val = l["value"].to_string();
        let val = if val.as_str() == "\"None\"" {
            "00".to_string()
        } else {
            val.as_str().replace("Some(", "").replace(")", "").replacen("\"", "\"0x", 1).to_string()
        };

        let value = map.get(&key);
        // 第一次出现的才插入，后续再次出现的不插入
        if value.is_none() {
            map.insert(key, val);
        }
    }
    let mut result: Vec<(&String, &String)> = map.iter().collect();
    result.sort_by(|(a1,_), (a2,_)| {
        let cmp1 = a1.cmp(&a2);
        let cmp = a1.len().cmp(&a2.len());
        let cmp2 = cmp.then(cmp1);
        cmp2
    });
    let mut buffer = String::from("{");
    for (k,v) in result {
        let k = k.replacen("\"", "\"0x", 1);
        let kk = k.replace("\"", "");
        if append_keys.contains(kk.as_str()) {
            // buffer.push_str(&format!("{}:null,", k));
            continue;
        }
        if v.as_str().eq("00") {
            // println!("{}: null,", k);
            buffer.push_str(&format!("{}:null,", k));
        } else {
            // let v = v.replacen("01","0x", 1);
            buffer.push_str(&format!("{}:{},", k, v));
            // println!("{}: {},", k, );
        }
        ws_all_keys.insert(kk);
    }
    buffer.pop();
    buffer.push('}');
    // println!("{}", buffer);
    (ws_all_keys, append_keys, buffer)
}

fn main() {
    println!("Substrate Validator Script!");
    let mut rt = tokio::runtime::Runtime::new().unwrap();
    let future = test_substrate_validate_changes();
    let _ = rt.block_on(future);
}

