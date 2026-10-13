//! Test fixture library importing `wasi:http/outgoing-handler`: `get(url)`
//! performs an HTTP GET and returns the response body trimmed. Loading it
//! without a `net: [hosts]` grant must be a hard error; with the grant only
//! allowlisted hosts are reachable (the host checks every request's
//! destination against the allowlist).

use perfscale_library_sdk::{args, export_library, Ctx, Error, FunctionInfo, Library};

#[derive(Default)]
struct NetFetch;

impl Library for NetFetch {
    fn functions(&self) -> Vec<FunctionInfo> {
        vec![FunctionInfo {
            name: "get",
            description: "HTTP GET a URL (destination host must be net-allowlisted)",
            secret: false,
        }]
    }

    fn call(
        &mut self,
        _ctx: &mut Ctx,
        func: &str,
        args: Vec<serde_json::Value>,
    ) -> Result<String, Error> {
        match func {
            "get" => {
                let url = args::string(&args, 0, func)?;
                wstd::runtime::block_on(async move {
                    let request = wstd::http::Request::get(&url)
                        .body(wstd::http::Body::empty())
                        .map_err(|e| Error::new(format!("get({url}): {e}")))?;
                    let mut response = wstd::http::Client::new()
                        .send(request)
                        .await
                        .map_err(|e| Error::new(format!("get({url}): {e}")))?;
                    let body = response
                        .body_mut()
                        .contents()
                        .await
                        .map_err(|e| Error::new(format!("get({url}): {e}")))?;
                    String::from_utf8(body.to_vec())
                        .map(|s| s.trim().to_string())
                        .map_err(|e| Error::new(format!("get({url}): body is not UTF-8: {e}")))
                })
            }
            other => Err(Error::new(format!("netfetch.{other}: unknown function"))),
        }
    }
}

export_library!(NetFetch);
