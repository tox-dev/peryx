#[cfg(unix)]
#[test]
fn killing_a_reaped_process_group_reports_the_missing_group() {
    use std::os::unix::process::CommandExt as _;

    let mut command = super::Command::new("sh");
    command.args(["-c", "exit 0"]).process_group(0);
    let mut process = command.spawn().expect("the fixture starts");
    process.wait().expect("the fixture exits");
    assert_eq!(
        super::kill_process_group(&process)
            .expect_err("a reaped group cannot be signalled")
            .raw_os_error(),
        Some(rustix::io::Errno::SRCH.raw_os_error())
    );
}
