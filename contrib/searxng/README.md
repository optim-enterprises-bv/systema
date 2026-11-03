# Local web search for Systema (SearXNG)

`web_search` queries a [SearXNG](https://github.com/searxng/searxng) instance — a self-hosted,
privacy-respecting metasearch engine — so searches stay local and keyless. `web_extract` then reads
a result page. With no `search_endpoint` configured, `web_search` just returns a clear error and the
other tools keep working.

## Run SearXNG (Docker)
```sh
docker run -d --name searxng -p 8888:8080 \
  -e "SEARXNG_BASE_URL=http://127.0.0.1:8888/" \
  searxng/searxng:latest
```
Enable the JSON API — in the instance's `settings.yml` add `json` to `search.formats`:
```yaml
search:
  formats:
    - html
    - json
```
(and restart the container). Quick check:
```sh
curl "http://127.0.0.1:8888/search?q=rust+async&format=json" | head -c 200
```

## Wire it into Systema
```toml
# ~/.systema/systema.toml
search_endpoint = "http://127.0.0.1:8888"
```
Restart the node. Any agent with `network = true` can now `web_search` and `web_extract`:
```sh
systema ask researcher "Search the web for the latest libp2p release and summarize what changed."
```

## Notes
- SearXNG is a **trusted local service**, so it's exempt from the public-IP guard that `fetch_url`
  applies — but its results are still treated as untrusted web content (the Rule of Two applies).
- Any endpoint that answers `GET /search?q=…&format=json` with `{ "results": [ {title,url,content} ] }`
  works; SearXNG is just the easy default.
