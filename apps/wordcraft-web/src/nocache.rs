//! kosmos: requests of the app that must not be answered from the browser's cache (an old version
//! of a document carries its old ETag, and saving it then ends in a conflict). Every request goes
//! out with `cache: no-store`; where the server allows it, the time is in the URL as well (`t`), as
//! some caches ignore the header.
//!
//! The Graph API is the exception: it answers an unknown query parameter with 400
//! (`Query parameter 't' is not supported`), which broke the place picker of Save As. Its URLs get
//! no `t`; `no-store` alone keeps them out of the cache.

/// `url` with `t=<now_ms>` added, unless it is a Graph API URL. A fragment stays at the end.
pub fn stamped(url: &str, now_ms: u64) -> String {
    let (address, fragment) = url.split_once('#').map_or((url, None), |(address, fragment)| (address, Some(fragment)));
    if is_graph(address) {
        return url.to_string();
    }
    let separator = if address.contains('?') { '&' } else { '?' };
    let stamped = format!("{address}{separator}t={now_ms}");
    match fragment {
        Some(fragment) => format!("{stamped}#{fragment}"),
        None => stamped,
    }
}

/// Whether `address` is on the Graph API (`{origin}/graph/…`), the path's first segment.
fn is_graph(address: &str) -> bool {
    let without_scheme = address.split_once("://").map_or(address, |(_, rest)| rest);
    let path = without_scheme.split_once('/').map_or("", |(_, path)| path);
    let path = path.split(['?', '#']).next().unwrap_or("");
    path == "graph" || path.starts_with("graph/")
}

#[cfg(test)]
mod tests {
    use super::stamped;

    #[test]
    fn webdav_events_and_tickets_get_the_time_and_graph_does_not() {
        assert_eq!(stamped("https://cloud.example.org/dav/spaces/d1/a.docx", 7), "https://cloud.example.org/dav/spaces/d1/a.docx?t=7");
        assert_eq!(stamped("https://cloud.example.org/signal/ticket?x=1", 7), "https://cloud.example.org/signal/ticket?x=1&t=7");
        assert_eq!(stamped("https://cloud.example.org/dav/a.docx#part", 7), "https://cloud.example.org/dav/a.docx?t=7#part");
        // Graph rejects unknown query parameters (400): Save As lists drives and folders there.
        for graph in [
            "https://cloud.example.org/graph/v1.0/me/drives",
            "https://cloud.example.org/graph/v1.0/drives/d1/items/i1/children?$select=name",
            "https://cloud.example.org/graph/v1beta1/drives",
        ] {
            assert_eq!(stamped(graph, 7), graph);
        }
        // Only the first path segment counts.
        assert_eq!(stamped("https://cloud.example.org/dav/graph/x", 7), "https://cloud.example.org/dav/graph/x?t=7");
        assert_eq!(stamped("https://cloud.example.org/graphics.png", 7), "https://cloud.example.org/graphics.png?t=7");
    }
}
