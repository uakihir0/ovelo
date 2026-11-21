//! print v2 signer verification result for an APK path
fn main() {
    let p = std::env::args().nth(1).unwrap();
    let b = std::fs::read(p).unwrap();
    println!(
        "has_v2={} has_v3={}",
        ovelo_apk::sig::has_v2_signature(&b),
        ovelo_apk::sig::has_v3_signature(&b)
    );
    match ovelo_apk::verify_v2(&b) {
        Ok(infos) => {
            for s in infos {
                println!(
                    "signer alg=0x{:x} subj={} fp={}",
                    s.sig_alg,
                    s.subject,
                    &s.cert_sha256[..16]
                );
            }
        }
        Err(e) => println!("ERR {}", e),
    }
}
