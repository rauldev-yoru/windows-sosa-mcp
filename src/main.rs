use rmcp::{ServiceExt, transport::stdio};
use windows_sosa_mcp::server::SosaServer;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!(
            "{} {}",
            windows_sosa_mcp::SERVER_NAME,
            windows_sosa_mcp::SERVER_VERSION
        );
        return Ok(());
    }
    let mut args = std::env::args().skip(1);
    let first = args.next();
    if first.as_deref() == Some(windows_sosa_mcp::tools::jobs::SUPERVISOR_ARG) {
        let dir = args.next().ok_or("missing job dir")?;
        std::process::exit(windows_sosa_mcp::tools::jobs::supervise(
            std::path::Path::new(&dir),
        ));
    }
    if first.as_deref() == Some(windows_sosa_mcp::handsfree::session::SESSION_ARG) {
        // The detached hands-free session (D25): hooks + overlay live and
        // die with this process, so a crash always gives input back.
        let dir = args.next().ok_or("missing job dir")?;
        std::process::exit(windows_sosa_mcp::handsfree::session::run(
            std::path::Path::new(&dir),
        ));
    }
    let _ = windows_sosa_mcp::audit::session_event("start");
    // Per-Monitor-V2 DPI awareness before any window/UIA/GDI call (D20).
    #[cfg(windows)]
    windows_sosa_mcp::ui::win::init_process();
    // stdout carries JSON-RPC only; diagnostics go to stderr.
    let server = SosaServer::new()?;
    let service = server.serve(stdio()).await?;
    let r = service.waiting().await;
    let _ = windows_sosa_mcp::audit::session_event("stop");
    r?;
    Ok(())
}
