use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[test]
#[ignore = "requires Linux x86_64 with read/write access to /dev/kvm"]
fn guest_executes_mov_and_hlt() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_kvm-from-scratch"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start the learning VMM");
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait().expect("check VMM status").is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let output = child.wait_with_output().expect("reap the timed-out VMM");
            panic!(
                "guest did not halt within 10 seconds: {}",
                String::from_utf8_lossy(&output.stderr),
            );
        }
        thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().expect("collect VMM output");
    assert!(
        output.status.success(),
        "VMM failed: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("Guest halted: RAX = 42"));
}
