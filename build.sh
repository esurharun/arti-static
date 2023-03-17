rustup target add x86_64-unknown-linux-musl
RUSTFLAGS='-C link-arg=-s' cargo build -p arti --release --target x86_64-unknown-linux-musl
