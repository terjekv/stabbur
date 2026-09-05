use std::{
    fs::OpenOptions,
    io::Write,
    path::Path,
    process::{Command, Output},
};

fn write_password(path: &Path, password: &str) {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).expect("password file must be created");
    writeln!(file, "{password}").expect("password file must be written");
}

fn admin_command(data_dir: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_stabbur-server"))
        .args(["admin"])
        .args(arguments)
        .args(["--data-dir", data_dir.to_str().unwrap()])
        .output()
        .expect("administrative command must start")
}

#[test]
fn local_bootstrap_initializes_an_empty_data_directory_without_a_secret_file() {
    let temporary = tempfile::tempdir().unwrap();
    let data_dir = temporary.path().join("data");
    let password_file = temporary.path().join("initial.password");
    let password = "initial automation password";
    write_password(&password_file, password);

    let bootstrap = admin_command(
        &data_dir,
        &[
            "bootstrap",
            "--username",
            "admin",
            "--password-file",
            password_file.to_str().unwrap(),
        ],
    );
    assert!(
        bootstrap.status.success(),
        "local bootstrap failed: {}",
        String::from_utf8_lossy(&bootstrap.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&bootstrap.stdout).trim(),
        "administrator created"
    );
    assert!(!data_dir.join("bootstrap.secret").exists());
    assert!(!String::from_utf8_lossy(&bootstrap.stdout).contains(password));
    assert!(!String::from_utf8_lossy(&bootstrap.stderr).contains(password));

    let repeated = admin_command(
        &data_dir,
        &[
            "bootstrap",
            "--username",
            "other",
            "--password-file",
            password_file.to_str().unwrap(),
        ],
    );
    assert!(!repeated.status.success());

    let replacement_file = temporary.path().join("replacement.password");
    write_password(&replacement_file, "replacement automation password");
    let reset = admin_command(
        &data_dir,
        &[
            "reset-password",
            "admin",
            "--password-file",
            replacement_file.to_str().unwrap(),
        ],
    );
    assert!(
        reset.status.success(),
        "local password reset failed: {}",
        String::from_utf8_lossy(&reset.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&reset.stdout).trim(),
        "password reset and credentials revoked"
    );
}
