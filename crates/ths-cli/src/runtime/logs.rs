use super::*;

impl Runtime {
    pub fn logs(
        &self,
        name: &InstanceName,
        service: Option<&str>,
        follow: bool,
        tail: Option<u64>,
        head: Option<u64>,
    ) -> Result<()> {
        let service = service.unwrap_or("app");
        let container = format!("{}-{service}", prefix(name));
        if let Some(lines) = head {
            return print_log_head(&container, lines);
        }
        let args = logs_args(&container, follow, tail);
        docker_inherit(&args.iter().map(String::as_str).collect::<Vec<_>>())
    }
}

pub(super) fn logs_args(container: &str, follow: bool, tail: Option<u64>) -> Vec<String> {
    let mut args = vec!["logs".to_owned()];
    if follow {
        args.push("--follow".to_owned());
    }
    if let Some(lines) = tail {
        args.extend(["--tail".to_owned(), lines.to_string()]);
    }
    args.push(container.to_owned());
    args
}

/// docker has no head option, so stream both of its output streams through
/// one pipe and stop the `docker logs` process once enough lines have been printed.
pub(super) fn print_log_head(container: &str, lines: u64) -> Result<()> {
    let (reader, writer) = std::io::pipe().context("creating log pipe")?;
    let mut child = Command::new("docker")
        .args(["logs", container])
        .stdout(writer.try_clone()?)
        .stderr(writer)
        .spawn()
        .context("running Docker")?;
    let printed = copy_lines(
        std::io::BufReader::new(reader),
        std::io::stdout().lock(),
        lines,
    )?;
    if printed == lines {
        child.kill().context("stopping docker logs")?;
    }
    // a killed `docker logs` leaves no exit code, so only its own failures count
    if child.wait()?.code().is_some_and(|code| code != 0) {
        bail!("docker logs {container} failed");
    }
    Ok(())
}

pub(super) fn copy_lines(
    mut reader: impl std::io::BufRead,
    mut out: impl std::io::Write,
    limit: u64,
) -> std::io::Result<u64> {
    let mut line = Vec::new();
    let mut copied = 0;
    while copied < limit {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        out.write_all(&line)?;
        copied += 1;
    }
    out.flush()?;
    Ok(copied)
}
