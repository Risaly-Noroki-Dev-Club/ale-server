# Server-owned browser reading tasks

`POST /v1/tasks/browser` is an opt-in, loopback-only synchronous task endpoint.
The caller supplies a goal and start URL; `ale-server` owns the headless Firefox
session, model decisions, observed-link navigation, full DOM text extraction,
screenshots and Markdown output. It uses the existing `RemoteCloudApi` Responses
adapter. This is a separate read-only browser adapter, not the desktop UIA task
harness or a demonstration of screenshot-based GUI grounding.

The default is disabled. When enabled, the HTTP listener must bind to loopback,
WebDriver must be loopback, and the operator must configure exact HTTPS hosts.
**Loopback-only is enforced by peer IP, not authentication:** a reverse proxy or
SSH tunnel terminating on `127.0.0.1` makes remote clients look local. Never
publish this route through a proxy/tunnel, and treat the browser profile as
untrusted (dedicated profile, trusted public hosts only).

Prerequisites on the service host: a headless **Firefox** plus a matching
**geckodriver** reachable at `webdriver_url` (the session is created with
`browserName=firefox` and `-headless`). Without them WebDriver session creation
fails and every task returns `502 BROWSER_TASK_FAILED`.

### Navigation rule

A task opened on a landing page must click observed links and wait for a real URL
transition before finishing. A task whose `start_url` already **is** the article
page may finish without any navigation: the observed page URL must then equal
`start_url`. `finish` is accepted only for the currently observed page
(`source_url` must match `page.url`), with `article_present=true`,
`truncated=false`, non-empty full text and Markdown that contains the source
URL, starts with `#`, and is corroborated by the observed page: it must mention
the observed `title`/`h1` or contain a verbatim slice of the article text. The
`latest_evidence` string must cite a publication date actually observed on the
page (a `<time datetime>` date or its visible text); pages without any `<time>`
metadata keep the weaker non-empty requirement.
The request must explicitly approve cloud disclosure and read-only navigation.
Link clicks wait for an actual URL transition and handle a newly opened tab
before supplying the next observation to the model. A click without a verified
transition times out rather than being blindly repeated. Every navigation is checked against the host grant and
`AutomationService::validate_plan`; `device.automation.allow_process_actions`
must permit opening URLs. No model JavaScript, arbitrary output paths, form
submission, downloads or device automation plans are accepted. Page and action
budgets are configured rather than inferred from model output. Top-level URLs
are checked before navigation and after redirects; this is not a network
sandbox for subresources or redirects, so use a dedicated browser profile and
trusted public hosts.

Example isolated configuration (leave other inference/device services disabled
when testing this endpoint on its own):

```toml
[server]
bind = "127.0.0.1:18766"
[engine]
enabled = false
[device]
enabled = false
[device.automation]
allow_process_actions = true
[browser_tasks]
enabled = true
webdriver_url = "http://127.0.0.1:18764"
model_url = "http://127.0.0.1:18761"
model = "gpt-6.1-sol"
api_key_env = "AME_BROWSER_API_TOKEN"
allowed_hosts = ["risnordev.org"]
output_dir = "/tmp/ame-browser-tasks"
max_steps = 16
max_concurrent = 1
task_timeout_s = 600
call_timeout_s = 90
max_request_bytes = 16384
max_response_bytes = 8388608
max_page_chars = 100000
max_markdown_bytes = 65536
max_output_tokens = 8192
artifact_retention_s = 604800   # prune task directories older than 7 days (0 = keep forever)
```

`model_url` may point to an SSH reverse tunnel with an ephemeral local-proxy
capability instead of copying the provider credential to the server. Reasoning
effort is currently selected by that model proxy; this endpoint does not expose
a user-controlled reasoning parameter. All configuration leaves support the
usual `ALE_SERVER__BROWSER_TASKS__...` environment overrides.

```json
{
  "goal": "访问网站最新的一篇文章，读取全文，输出中文 Markdown 总结。",
  "start_url": "https://risnordev.org/",
  "allow_cloud": true,
  "approve_read_only": true
}
```

A completed response includes `task_id`, `artifact_dir`, `source_url`,
`latest_evidence`, `markdown_path`, `full_text_chars` and `markdown_bytes`.

The model can also decline to continue; that is not an error. `need_help`
returns HTTP **200** with the same task metadata and
`{"status": "needs_user", "reason": "<model reason>"}` (no Markdown is written,
`artifact_dir` still contains the observations collected so far).
Artifacts include the submitted request, each actual page observation and
screenshot, model decisions, result and `summary.md`. Source text is not silently
truncated: a page exceeding the configured limit fails instead of claiming a
full-article summary. The model must finish on an observed article with an
original URL and latest-selection evidence. The server writes the Markdown and
checks byte-for-byte readback before returning `completed`.

Latest-article selection and summary quality remain model judgments, recorded
for review; this is not an independently exhaustive chronological website
crawl. The route supports article-shaped pages, not arbitrary applications.
HTTP 429 means capacity is occupied; disabled instances return 503;
missing approval or a non-loopback caller returns 403. Browser/model failures,
invalid decisions, failed evidence checks and exhausted budgets return
`BROWSER_TASK_FAILED` with HTTP 502, and the server closes the WebDriver session
on completion, failure or task timeout. Request-level rejections use the normal
`{"error": {"code": ...}}` envelope: malformed/unknown JSON → `422 INVALID_JSON`,
body over `max_request_bytes` → `413 PAYLOAD_TOO_LARGE`.

`task_timeout_s` bounds the whole task (observations + model calls + link
navigation). `call_timeout_s` bounds each WebDriver command and each model call,
including the wait for a clicked link to produce a URL transition.

### Artifact retention

Every task writes a fresh directory under `output_dir` (screenshots, full page
observations, model decisions, `request.json`, `result.json`, `summary.md`).
Set `artifact_retention_s` to have the server delete directories whose last
modification is older than that value; pruning runs at startup and after every
task. `0` disables pruning (artifacts then grow without bound).
Artifacts deliberately contain public page text and summary content. Do not use
this diagnostic reader with authenticated/private pages without an appropriate
retention policy (`artifact_retention_s`). Credentials are read only from the
named environment variable and are never stored in artifacts or server logs.

### Trust boundary for page content

Observations are extracted by JavaScript running **inside the page**. A page can
therefore forge the observation fields it reports (`text`, `links`,
`article_present`, `truncated`, `times`); only the document URL is kept by the
browser itself. Forged links cannot escape the host grant (clicks are resolved
against the live DOM and every navigation is re-checked), and no model-supplied
JavaScript is executed, but treat every granted host as trusted content.
