//! Debug tool: fetch a single article and dump raw bytes + decode result.

use std::io::Write;

use nobz_core::nntp::{NntpClient, ServerConfig};
use nobz_core::nzb;
use nobz_core::yenc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let nzb_path = std::env::args().nth(1).expect("usage: dump-article <nzb>");
    let host = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "news.eweka.nl".into());
    let user = std::env::args()
        .nth(3)
        .expect("usage: dump-article <nzb> <host> <user> <pass>");
    let pass = std::env::args()
        .nth(4)
        .expect("usage: dump-article <nzb> <host> <user> <pass>");

    let cfg = ServerConfig {
        host,
        port: 563,
        tls: true,
        user: Some(user),
        password: Some(pass),
        max_connections: 1,
        priority: 0,
    };
    let mut client = NntpClient::connect(&cfg).await?;

    let nzb_bytes = std::fs::read(&nzb_path)?;
    let nzb_doc = nzb::parse(&nzb_bytes)?;

    // Find first non-par2 file
    let file = nzb_doc
        .files
        .iter()
        .find(|f| !f.filename().ends_with(".par2"))
        .unwrap_or(&nzb_doc.files[0]);
    let seg = &file.segments[0];
    println!("File: {}", file.filename());
    println!("Segment #{}: {}", seg.number, seg.message_id);

    let body = client.body(&seg.message_id).await?.unwrap();
    println!("Raw body: {} bytes", body.bytes.len());

    // Dump first 300 bytes as hex
    let preview = &body.bytes[..body.bytes.len().min(300)];
    println!("\nFirst 300 bytes:");
    for (i, chunk) in preview.chunks(16).enumerate() {
        let hex: Vec<String> = chunk.iter().map(|b| format!("{b:02x}")).collect();
        let ascii: String = chunk
            .iter()
            .map(|&b| {
                if (32..=126).contains(&b) {
                    b as char
                } else {
                    '.'
                }
            })
            .collect();
        println!("{:04x}: {:48} {}", i * 16, hex.join(" "), ascii);
    }

    // Dump last 200 bytes
    let tail_start = body.bytes.len().saturating_sub(200);
    println!("\nLast 200 bytes:");
    for (i, chunk) in body.bytes[tail_start..].chunks(16).enumerate() {
        let hex: Vec<String> = chunk.iter().map(|b| format!("{b:02x}")).collect();
        let ascii: String = chunk
            .iter()
            .map(|&b| {
                if (32..=126).contains(&b) {
                    b as char
                } else {
                    '.'
                }
            })
            .collect();
        println!(
            "{:04x}: {:48} {}",
            tail_start + i * 16,
            hex.join(" "),
            ascii
        );
    }

    // Save raw bytes
    let mut f = std::fs::File::create("/tmp/opencode/article_raw.bin")?;
    f.write_all(&body.bytes)?;

    // Decode
    match yenc::decode_article(&body.bytes) {
        Ok(decoded) => {
            println!(
                "\nDecoded: {} bytes, crc_ok={}, crc_unknown={}, computed_crc={:#010x}",
                decoded.data.len(),
                decoded.crc_ok,
                decoded.crc_unknown,
                decoded.crc32
            );
            println!(
                "begin={}, end={}, total_size={}",
                decoded.begin, decoded.end, decoded.total_size
            );
        }
        Err(e) => println!("\nDecode error: {e}"),
    }

    Ok(())
}
