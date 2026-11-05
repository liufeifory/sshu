use sshu::term::Console;

fn main() {
    let console = match Console::open() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };

    let (w, h) = console.size();
    let _ = console.write_all(format!("SIZE {w}x{h}\r\n").as_bytes());

    let mut buf = [0u8; 1024];
    loop {
        match console.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let _ = console.write_all(&buf[..n]);
                if buf[..n].contains(&b'q') {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let _ = console.write_all(b"\r\nBYE\r\n");
}
