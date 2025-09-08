use actix_web::{App, HttpResponse, HttpServer, Responder, get, post, web};
use anyhow::{Context, Result, anyhow};
use rand::{Rng, rng};
use reqwest::header::{
    ACCEPT, ACCEPT_LANGUAGE, CACHE_CONTROL, CONNECTION, DNT, HeaderMap, HeaderName, HeaderValue,
    PRAGMA, REFERER, UPGRADE_INSECURE_REQUESTS, USER_AGENT,
};
use robotstxt::DefaultMatcher;
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use serde_json;
use std::{collections::HashSet, time::Duration};
use tokio::{task::yield_now, time::sleep};
use url::Url;

// for SSE streaming
use bytes::Bytes;
use tokio::sync::mpsc;

// -------------------------
// Request / Response Types
// -------------------------

#[derive(Deserialize)]
struct ScrapeReq {
    /// Category URL, with or without ?page=N. We'll start from that page and auto-iterate.
    url: String,
    /// Optional page cap; if omitted we use HARD_PAGE_CAP.
    page_range: Option<usize>,
}

#[derive(Deserialize)]
struct ScrapeQuery {
    url: String,
    page_range: Option<usize>,
}

#[derive(Serialize, Clone)]
struct PriceHit {
    id: String,
    listing_url: String,
    title: String,
    price_numeric: Option<f64>,
    currency: Option<String>,
    raw_price: String,
    sqm: Option<f64>,
    price_per_m2: Option<f64>,
}

#[derive(Serialize)]
struct Meta {
    page_count: usize,
    total_hits: usize,
    next_url: Option<String>,
}

#[derive(Serialize)]
struct ApiResponse {
    hits: Vec<PriceHit>,
    meta: Meta,
}

// -------------------------
// HTTP Handlers
// -------------------------

#[get("/")]
async fn index() -> impl Responder {
    HttpResponse::Ok().body(
        "Claw online.\n\
         JSON:\n  POST /scrape {\"url\":\"https://www.njuskalo.hr/prodaja-stanova/zagreb\",\"page_range\":10}\n  GET  /scrape?url=...&page_range=10\n\
         Stream:\n  GET  /scrape/stream?url=...&page_range=10 (SSE)\n\
         UI:\n  GET  /dashboard",
    )
}

#[get("/healthz")]
async fn healthz() -> impl Responder {
    HttpResponse::Ok().body("ok")
}

#[post("/scrape")]
async fn scrape_endpoint(body: web::Json<ScrapeReq>) -> impl Responder {
    match scrape_prices(&body.url, body.page_range).await {
        Ok((hits, meta)) => HttpResponse::Ok().json(ApiResponse { hits, meta }),
        Err(e) => {
            let err = serde_json::json!({ "error": format!("{e:#}") });
            HttpResponse::BadRequest().json(err)
        }
    }
}

#[get("/scrape")]
async fn scrape_get(q: web::Query<ScrapeQuery>) -> impl Responder {
    match scrape_prices(&q.url, q.page_range).await {
        Ok((hits, meta)) => HttpResponse::Ok().json(ApiResponse { hits, meta }),
        Err(e) => {
            let err = serde_json::json!({ "error": format!("{e:#}") });
            HttpResponse::BadRequest().json(err)
        }
    }
}

// --------------
// SSE streaming
// --------------

#[derive(Deserialize)]
struct StreamParams {
    url: String,
    page_range: Option<usize>,
}

fn sse_event(event: &str, data_json: &str) -> Bytes {
    let payload = format!("event: {}\ndata: {}\n\n", event, data_json);
    Bytes::from(payload)
}

#[get("/scrape/stream")]
async fn scrape_stream(q: web::Query<StreamParams>) -> impl Responder {
    let (tx, mut rx) = mpsc::channel::<Bytes>(32);
    let url = q.url.clone();
    let max_pages_opt = q.page_range;

    actix_web::rt::spawn(async move {
        // validate once
        let parsed = match Url::parse(&url) {
            Ok(u) => u,
            Err(e) => {
                let _ = tx
                    .send(sse_event("error", &format!(r#"{{"error":"{}"}}"#, e)))
                    .await;
                return;
            }
        };
        let host = match parsed.host_str() {
            Some(h) => h.to_string(),
            None => {
                let _ = tx
                    .send(sse_event("error", r#"{"error":"url has no host"}"#))
                    .await;
                return;
            }
        };
        let allowed: HashSet<&'static str> = HashSet::from(["www.njuskalo.hr", "njuskalo.hr"]);
        if !allowed.contains(host.as_str()) {
            let _ = tx
                .send(sse_event("error", r#"{"error":"domain not in whitelist"}"#))
                .await;
            return;
        }

        // robots.txt
        let robots_url = format!("{}://{}/robots.txt", parsed.scheme(), host);
        let robots_txt = match reqwest::get(&robots_url).await {
            Ok(rsp) => rsp.text().await.unwrap_or_default(),
            Err(_) => String::new(),
        };
        let mut robots_matcher: DefaultMatcher = DefaultMatcher::default();
        if !robots_matcher.one_agent_allowed_by_robots(&robots_txt, "Mozilla", &url) {
            let _ = tx
                .send(sse_event(
                    "error",
                    r#"{"error":"robots.txt disallows this URL"}"#,
                ))
                .await;
            return;
        }

        let (base, mut page) = normalize_pager(&parsed);
        let host = parsed.host_str().unwrap_or_default().to_string();
        let origin = format!("{}://{}", base.scheme(), host);
        let mut prev_page_url: Option<Url> = None;

        let max_pages = max_pages_opt.unwrap_or(HARD_PAGE_CAP);
        let _ = tx
            .send(sse_event(
                "start",
                &format!(r#"{{"origin":"{}","max_pages":{}}}"#, origin, max_pages),
            ))
            .await;

        // selectors
        let list_section = Selector::parse("section.EntityList").unwrap();
        let list_ul = Selector::parse("ul.EntityList-items").unwrap();
        let li_item = Selector::parse("li.EntityList-item").unwrap();
        let body_sel = Selector::parse("article.entity-body").unwrap();
        let title_a = Selector::parse("h3.entity-title > a.link").unwrap();
        let price_sel = Selector::parse("div.entity-prices strong.price").unwrap();
        let desc_main = Selector::parse(".entity-description-main").unwrap();

        let mut pages = 0usize;
        let mut total_hits = 0usize;

        loop {
            if pages >= max_pages {
                let _ = tx
                    .send(sse_event(
                        "done",
                        &format!(r#"{{"pages":{},"total_hits":{}}}"#, pages, total_hits),
                    ))
                    .await;
                break;
            }

            let page_url = match build_page_url(&base, page) {
                Ok(u) => u,
                Err(e) => {
                    let _ = tx
                        .send(sse_event("error", &format!(r#"{{"error":"{}"}}"#, e)))
                        .await;
                    break;
                }
            };
            pages += 1;

            // new client per page
            let client = match reqwest::Client::builder()
                .user_agent(random_desktop_ua())
                .redirect(reqwest::redirect::Policy::limited(8))
                .timeout(Duration::from_secs(25))
                .build()
            {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx
                        .send(sse_event("error", &format!(r#"{{"error":"{}"}}"#, e)))
                        .await;
                    break;
                }
            };

            warmup_hit(&client, &origin).await;

            let referer = prev_page_url
                .as_ref()
                .map(|u| u.as_str().to_string())
                .unwrap_or_else(|| origin.clone());

            let html = match retry_fetch_html(&client, &page_url, &referer).await {
                Ok(h) => h,
                Err(e) => {
                    let _ = tx
                        .send(sse_event("error", &format!(r#"{{"error":"{}"}}"#, e)))
                        .await;
                    break;
                }
            };

            let doc = Html::parse_document(&html);
            let mut page_hits: Vec<PriceHit> = Vec::new();
            for section in doc.select(&list_section) {
                for ul in section.select(&list_ul) {
                    for li in ul.select(&li_item) {
                        if let Some(hit) =
                            parse_card(&li, &page_url, &body_sel, &title_a, &price_sel, &desc_main)
                        {
                            page_hits.push(hit);
                        }
                    }
                }
            }
            if page_hits.is_empty() {
                for li in doc.select(&li_item) {
                    if let Some(hit) =
                        parse_card(&li, &page_url, &body_sel, &title_a, &price_sel, &desc_main)
                    {
                        page_hits.push(hit);
                    }
                }
            }

            total_hits += page_hits.len();
            let payload = serde_json::json!({
                "page": page,
                "url": page_url.as_str(),
                "count": page_hits.len(),
                "hits": page_hits,
                "total_hits_so_far": total_hits
            });
            let _ = tx.send(sse_event("page", &payload.to_string())).await;

            if page_hits.is_empty() {
                let _ = tx
                    .send(sse_event(
                        "done",
                        &format!(r#"{{"pages":{},"total_hits":{}}}"#, pages, total_hits),
                    ))
                    .await;
                break;
            }

            prev_page_url = Some(page_url);
            page += 1;

            sleep(Duration::from_millis(rng().random_range(900..2200))).await;
            let _ = yield_now();
        }
    });

    let stream = async_stream::stream! {
        while let Some(chunk) = rx.recv().await {
            yield Ok::<Bytes, actix_web::Error>(chunk);
        }
    };

    HttpResponse::Ok()
        .insert_header(("Content-Type", "text/event-stream"))
        .insert_header(("Cache-Control", "no-cache"))
        .insert_header(("Connection", "keep-alive"))
        .streaming(stream)
}

// -------------------------
// Tiny HTML dashboard
// -------------------------

#[get("/dashboard")]
async fn dashboard() -> impl Responder {
    HttpResponse::Ok()
        .insert_header(("Content-Type", "text/html; charset=utf-8"))
        .body(r#"
<!doctype html>
<html lang="en" class="dark">
<head>
  <meta charset="utf-8" />
  <title>Claw Dashboard</title>

  <!-- Tailwind (CDN) -->
  <script>
    tailwind.config = { darkMode: 'class' };
  </script>
  <script src="https://cdn.tailwindcss.com"></script>

  <!-- Alpine.js (CDN) -->
  <script defer src="https://unpkg.com/alpinejs@3.x.x/dist/cdn.min.js"></script>

  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <style>[x-cloak]{display:none!important}</style>
</head>
<body class="bg-slate-900 text-slate-100 antialiased">
  <!-- App fills the viewport height -->
  <main class="max-w-6xl mx-auto p-6 flex flex-col gap-6 h-dvh"
        x-data="flatwatch()"
        x-init="init()">

    <div class="flex items-center justify-between">
      <h1 class="text-3xl font-bold tracking-tight shrink-0">Claw Dashboard</h1>
      <!-- (no theme toggle anymore) -->
    </div>

    <!-- Controls -->
    <div class="bg-slate-800 shadow-sm ring-1 ring-slate-700 rounded-xl p-4 space-y-4 shrink-0">
      <div class="grid grid-cols-1 md:grid-cols-4 gap-3 items-center">
        <label class="md:col-span-1 text-sm font-medium text-slate-300">Category URL</label>
        <input x-model="url"
               type="text"
               class="md:col-span-3 w-full rounded-lg border-slate-700 bg-slate-900 text-slate-100 focus:border-indigo-500 focus:ring-indigo-500 px-2 py-1.5 text-sm"
               placeholder="https://www.njuskalo.hr/prodaja-stanova/zagreb">

        <label class="md:col-span-1 text-sm font-medium text-slate-300">page_range</label>
        <input x-model.number="pageRange"
               type="number" min="1" max="500"
               class="md:col-span-1 w-full rounded-lg border-slate-700 bg-slate-900 text-slate-100 focus:border-indigo-500 focus:ring-indigo-500 px-2 py-1.5 text-sm"
               placeholder="10">
        
        <div class="md:col-span-2 flex items-center gap-3">
        <button @click="start()"
                :disabled="isRunning"
                class="inline-flex items-center gap-2 px-2 py-1 text-sm rounded-md bg-indigo-600 text-white font-medium hover:bg-indigo-700 disabled:opacity-50 disabled:cursor-not-allowed">
            <svg x-show="!isRunning" xmlns="http://www.w3.org/2000/svg" class="h-3.5 w-3.5" fill="none" viewBox="0 0 24 24" stroke="currentColor"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M5 12h14M12 5l7 7-7 7"/></svg>
            <svg x-show="isRunning" xmlns="http://www.w3.org/2000/svg" class="animate-spin h-3.5 w-3.5" viewBox="0 0 24 24" fill="none"><circle class="opacity-30" cx="12" cy="12" r="10" stroke="currentColor" stroke-width="4"/><path class="opacity-80" fill="currentColor" d="M4 12a8 8 0 018-8v4a4 4 0 00-4 4H4z"/></svg>
            <span class="text-sm" x-text="isRunning ? 'Running…' : 'Start'"></span>
        </button>

        <!-- CSV export button -->
        <button @click="downloadCSV()"
                :disabled="rows.length === 0"
                class="inline-flex items-center gap-2 px-2 py-1 text-sm rounded-md bg-slate-700 text-slate-100 font-medium hover:bg-slate-600 disabled:opacity-50 disabled:cursor-not-allowed">
            <svg xmlns="http://www.w3.org/2000/svg" class="h-3.5 w-3.5" viewBox="0 0 24 24" fill="currentColor"><path d="M12 3a1 1 0 011 1v9.586l2.293-2.293a1 1 0 111.414 1.414l-4.007 4.007a1 1 0 01-1.414 0L7.279 12.707a1 1 0 111.414-1.414L11 13.586V4a1 1 0 011-1z"/><path d="M5 15a1 1 0 112 0v3h10v-3a1 1 0 112 0v3a2 2 0 01-2 2H7a2 2 0 01-2-2v-3z"/></svg>
            <span class="text-sm">Export CSV</span>
        </button>
        </div>
      </div>
      
    </div>

    <!-- Log (collapsed by default) -->
    <div class="bg-slate-800 shadow-sm ring-1 ring-slate-700 rounded-xl p-4 shrink-0">
      <div class="flex items-center justify-between">
        <div class="text-sm font-semibold text-slate-300">Log</div>
        <div class="text-sm text-slate-300 flex gap-4">
        <div><span class="font-semibold">Pages:</span> <span x-text="stats.pages"></span></div>
        <div><span class="font-semibold">Total hits:</span> <span x-text="stats.totalHits"></span></div>
        <div><span class="font-semibold">Last:</span> <span x-text="lastPageMsg || '-'"></span></div>
      </div>
        <button
          @click="logOpen = !logOpen"
          class="text-xs px-2 py-1 rounded-md bg-slate-700 text-slate-100 hover:bg-slate-600">
          <span x-text="logOpen ? 'Hide' : 'Show'"></span>
        </button>
      </div>
      <div x-show="logOpen" x-cloak class="mt-2">
        <pre id="log"
             class="h-36 overflow-auto whitespace-pre-wrap text-sm leading-relaxed text-slate-200 bg-slate-900/40 rounded-md p-2"
             x-text="logs.join('\n')"></pre>
      </div>
    </div>

    <!-- Results -->
    <div class="bg-slate-800 shadow-sm ring-1 ring-slate-700 rounded-xl p-4 flex-1 min-h-0 flex flex-col">
      <div class="flex-1 min-h-0 overflow-y-auto rounded-lg">
        <table class="min-w-full text-sm">
          <thead class="bg-slate-700 sticky top-0 z-10">
            <tr class="text-left text-slate-100">
              <th class="px-3 py-2 font-medium">#</th>
              <th class="px-3 py-2 font-medium">Page</th>
              <th class="px-3 py-2 font-medium">Title</th>
              <th class="px-3 py-2 font-medium">Price</th>
              <th class="px-3 py-2 font-medium">Currency</th>
              <th class="px-3 py-2 font-medium">m²</th>
              <th class="px-3 py-2 font-medium">€/m²</th>
              <th class="px-3 py-2 font-medium">URL</th>
            </tr>
          </thead>
          <tbody>
            <template x-for="row in rows" :key="row._k">
              <tr class="border-t border-slate-700 hover:bg-slate-700/50">
                <td class="px-3 py-2" x-text="row.idx"></td>
                <td class="px-3 py-2" x-text="row.page"></td>
                <td class="px-3 py-2"><span class="line-clamp-2" x-text="row.title"></span></td>
                <td class="px-3 py-2 tabular-nums" x-text="row.price_numeric ?? ''"></td>
                <td class="px-3 py-2" x-text="row.currency ?? ''"></td>
                <td class="px-3 py-2 tabular-nums" x-text="row.sqm ?? ''"></td>
                <td class="px-3 py-2 tabular-nums" x-text="row.price_per_m2_round ?? ''"></td>
                <td class="px-3 py-2">
                  <a class="text-indigo-400 hover:underline" :href="row.listing_url" target="_blank">open</a>
                </td>
              </tr>
            </template>
          </tbody>
        </table>
      </div>
    </div>
  </main>

  <script>
    function flatwatch() {
      return {
        // form state
        url: 'https://www.njuskalo.hr/prodaja-stanova/zagreb',
        pageRange: 10,

        // runtime state
        isRunning: false,
        rows: [],
        logs: [],
        stats: { pages: 0, totalHits: 0 },
        lastPageMsg: '',
        logOpen: false, // collapsed by default

        _es: null,
        _idx: 0,

        init() {},
        log(msg) {
          this.logs.push(msg);
          this.$nextTick(() => {
            const el = document.getElementById('log');
            if (el) el.scrollTop = el.scrollHeight;
          });
        },

        start() {
          if (!this.url) { this.log('Please enter a category URL.'); return; }
          if (this._es) { try { this._es.close(); } catch (_) {} this._es = null; }
          this.rows = [];
          this.logs = [];
          this.stats = { pages: 0, totalHits: 0 };
          this.lastPageMsg = '-';
          this._idx = 0;

          const qs = new URLSearchParams({ url: this.url, page_range: String(this.pageRange || 10) });
          const sseUrl = `/scrape/stream?${qs.toString()}`;
          this.log(`Connecting: ${sseUrl}`);
          this.isRunning = true;

          const es = new EventSource(sseUrl);
          this._es = es;

          es.addEventListener('start', (ev) => this.log(`START: ${ev.data}`));

          es.addEventListener('page', (ev) => {
            const data = JSON.parse(ev.data || '{}');
            const pageNo = data.page ?? '?';
            const hits = Array.isArray(data.hits) ? data.hits : [];
            this.stats.pages += 1;
            this.stats.totalHits += hits.length;
            this.lastPageMsg = `PAGE ${pageNo} (${hits.length} items)`;
            this.log(this.lastPageMsg);

            hits.forEach(h => {
              const pricePer = h.price_per_m2 ? Math.round(h.price_per_m2) : null;
              this.rows.push({
                _k: `${pageNo}-${h.id || Math.random()}`,
                idx: ++this._idx,
                page: pageNo,
                title: (h.title || '').replace(/</g, '&lt;'),
                price_numeric: h.price_numeric,
                currency: h.currency,
                sqm: h.sqm,
                price_per_m2_round: pricePer,
                listing_url: h.listing_url
              });
            });
          });

          es.addEventListener('done', (ev) => {
            this.log(`DONE: ${ev.data}`);
            this.isRunning = false;
            es.close();
            this._es = null;
          });

          es.addEventListener('error', (ev) => {
            this.log(`ERROR: ${(ev && ev.data) || '(connection error)'} — closing stream`);
            this.isRunning = false;
            es.close();
            this._es = null;
          });
        },

        // CSV export
        downloadCSV() {
          if (!this.rows.length) return;

          const headers = ['idx','page','title','price_numeric','currency','sqm','price_per_m2_round','listing_url'];
          const esc = (v) => {
            if (v === null || v === undefined) return '';
            const s = String(v);
            return /[",\n]/.test(s) ? `"${s.replace(/"/g, '""')}"` : s;
          };

          const lines = [
            headers.join(','),
            ...this.rows.map(r => headers.map(h => esc(r[h])).join(','))
          ];

          const blob = new Blob([lines.join('\n')], { type: 'text/csv;charset=utf-8;' });
          const url = URL.createObjectURL(blob);
          const a = document.createElement('a');
          a.href = url;
          a.download = `flatwatch_${new Date().toISOString().slice(0,19).replace(/[:T]/g,'-')}.csv`;
          document.body.appendChild(a);
          a.click();
          setTimeout(() => {
            document.body.removeChild(a);
            URL.revokeObjectURL(url);
          }, 0);
        },
      }
    }
  </script>
</body>
</html>
"#
)
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    eprintln!("Starting Claw on 0.0.0.0:8080 …");
    HttpServer::new(|| {
        App::new()
            .service(index)
            .service(healthz)
            .service(scrape_endpoint)
            .service(scrape_get) // GET JSON
            .service(scrape_stream) // SSE stream
            .service(dashboard) // Minimal UI
    })
    .bind(("0.0.0.0", 8080))?
    .run()
    .await
}

// -------------------------
// Core scraper (auto-paging; per-page client reset)
// -------------------------

const HARD_PAGE_CAP: usize = 200; // sanity guard

async fn scrape_prices(
    start_url: &str,
    page_range: Option<usize>,
) -> Result<(Vec<PriceHit>, Meta)> {
    let url = Url::parse(start_url).context("invalid url")?;
    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("url has no host"))?
        .to_string();
    let allowed: HashSet<&'static str> = HashSet::from(["www.njuskalo.hr", "njuskalo.hr"]);
    if !allowed.contains(host.as_str()) {
        return Err(anyhow!("domain not in whitelist"));
    }

    // robots.txt check
    let robots_url = format!("{}://{}/robots.txt", url.scheme(), host);
    let robots_txt = match reqwest::get(&robots_url).await {
        Ok(rsp) => rsp.text().await.unwrap_or_default(),
        Err(_) => String::new(),
    };
    let mut robots_matcher: DefaultMatcher = DefaultMatcher::default();
    if !robots_matcher.one_agent_allowed_by_robots(&robots_txt, "Mozilla", start_url) {
        return Err(anyhow!("robots.txt disallows this URL"));
    }

    let (base, mut page) = normalize_pager(&url);

    // selectors
    let list_section = Selector::parse("section.EntityList").unwrap();
    let list_ul = Selector::parse("ul.EntityList-items").unwrap();
    let li_item = Selector::parse("li.EntityList-item").unwrap();
    let body_sel = Selector::parse("article.entity-body").unwrap();
    let title_a = Selector::parse("h3.entity-title > a.link").unwrap();
    let price_sel = Selector::parse("div.entity-prices strong.price").unwrap();
    let desc_main = Selector::parse(".entity-description-main").unwrap();

    let mut hits: Vec<PriceHit> = Vec::new();
    let mut seen_ids: HashSet<String> = HashSet::new();
    let mut pages = 0usize;
    let mut last_next_url: Option<String> = None;
    let origin = format!("{}://{}", base.scheme(), host);
    let mut prev_page_url: Option<Url> = None;

    let max_pages = page_range.unwrap_or(HARD_PAGE_CAP);

    loop {
        if pages >= max_pages {
            eprintln!("[pager] reached max_pages={}, stopping.", max_pages);
            break;
        }

        let page_url = build_page_url(&base, page).context("build page url failed")?;
        pages += 1;

        // per-page client reset
        let client = reqwest::Client::builder()
            .user_agent(random_desktop_ua())
            .redirect(reqwest::redirect::Policy::limited(8))
            .timeout(Duration::from_secs(25))
            .build()?;

        warmup_hit(&client, &origin).await;

        let referer = prev_page_url
            .as_ref()
            .map(|u| u.as_str().to_string())
            .unwrap_or_else(|| origin.clone());

        let html = retry_fetch_html(&client, &page_url, &referer).await?;

        let probe = html.replace('\n', " ");
        eprintln!(
            "[{}] len={} has(EntityList)={} has(EntityList-item)={} url={} referer={}",
            page,
            probe.len(),
            probe.contains("EntityList"),
            probe.contains("EntityList-item"),
            page_url,
            referer
        );

        let doc = Html::parse_document(&html);

        // parse cards
        let mut page_count = 0usize;
        for section in doc.select(&list_section) {
            for ul in section.select(&list_ul) {
                for li in ul.select(&li_item) {
                    if let Some(hit) =
                        parse_card(&li, &page_url, &body_sel, &title_a, &price_sel, &desc_main)
                    {
                        if register_hit(hit, &mut hits, &mut seen_ids) {
                            page_count += 1;
                        }
                    }
                }
            }
        }

        if page_count == 0 {
            for li in doc.select(&li_item) {
                if let Some(hit) =
                    parse_card(&li, &page_url, &body_sel, &title_a, &price_sel, &desc_main)
                {
                    if register_hit(hit, &mut hits, &mut seen_ids) {
                        page_count += 1;
                    }
                }
            }
        }

        eprintln!(
            "[{}] page={} cards={} total_hits={}",
            page,
            page_url,
            page_count,
            hits.len()
        );

        if page_count == 0 {
            last_next_url = None;
            break;
        } else {
            last_next_url = Some(build_page_url(&base, page + 1)?.to_string());
            prev_page_url = Some(page_url);
            page += 1;
            sleep(Duration::from_millis(rng().random_range(900..2200))).await;
            let _ = yield_now();
        }
    }

    let meta = Meta {
        page_count: pages,
        total_hits: hits.len(),
        next_url: last_next_url,
    };
    Ok((hits, meta))
}

fn register_hit(hit: PriceHit, hits: &mut Vec<PriceHit>, seen: &mut HashSet<String>) -> bool {
    if !hit.id.is_empty() && !seen.insert(hit.id.clone()) {
        return false;
    }
    hits.push(hit);
    true
}

// -------------------------
// Fetch helpers
// -------------------------

#[derive(Clone, Copy, Debug)]
enum Profile {
    Desktop,
    Mobile,
}

fn base_headers(profile: Profile, referer: &str) -> HeaderMap {
    let mut h = HeaderMap::new();
    match profile {
        Profile::Desktop => {
            h.insert(
                USER_AGENT,
                HeaderValue::from_str(&random_desktop_ua()).unwrap(),
            );
            h.insert(
                ACCEPT,
                HeaderValue::from_static(
                    "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
                ),
            );
            h.insert(
                ACCEPT_LANGUAGE,
                HeaderValue::from_static("hr-HR,hr;q=0.9,en-US;q=0.8,en;q=0.7"),
            );
        }
        Profile::Mobile => {
            h.insert(
                USER_AGENT,
                HeaderValue::from_str(&random_mobile_ua()).unwrap(),
            );
            h.insert(
                ACCEPT,
                HeaderValue::from_static(
                    "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
                ),
            );
            h.insert(
                ACCEPT_LANGUAGE,
                HeaderValue::from_static("hr-HR,hr;q=0.9,en-US;q=0.8,en;q=0.7"),
            );
        }
    }
    h.insert(REFERER, HeaderValue::from_str(referer).unwrap());
    h.insert(UPGRADE_INSECURE_REQUESTS, HeaderValue::from_static("1"));
    h.insert(CONNECTION, HeaderValue::from_static("keep-alive"));
    h.insert(CACHE_CONTROL, HeaderValue::from_static("max-age=0"));
    h.insert(PRAGMA, HeaderValue::from_static("no-cache"));
    h.insert(DNT, HeaderValue::from_static("1"));

    h.insert(
        HeaderName::from_static("sec-fetch-site"),
        HeaderValue::from_static("same-origin"),
    );
    h.insert(
        HeaderName::from_static("sec-fetch-mode"),
        HeaderValue::from_static("navigate"),
    );
    h.insert(
        HeaderName::from_static("sec-fetch-dest"),
        HeaderValue::from_static("document"),
    );
    h
}

async fn warmup_hit(client: &reqwest::Client, origin: &str) {
    let headers = base_headers(Profile::Desktop, origin);
    match client.get(origin).headers(headers).send().await {
        Ok(r) => {
            let _ = r.text().await;
        }
        Err(e) => eprintln!("[warmup] failed: {e}"),
    }
}

async fn retry_fetch_html(
    client: &reqwest::Client,
    page_url: &Url,
    referer: &str,
) -> Result<String> {
    let mut attempts = 0;
    let mut last_err: Option<anyhow::Error> = None;
    let mut profile = Profile::Desktop;

    while attempts < 5 {
        attempts += 1;
        let headers = base_headers(profile, referer);
        let resp = client.get(page_url.as_str()).headers(headers).send().await;

        match resp {
            Ok(rsp) => {
                // Capture these BEFORE .text() (which consumes the response)
                let status = rsp.status();
                let final_url = rsp.url().clone();
                let text = rsp.text().await.unwrap_or_default();
                let len = text.len();

                eprintln!(
                    "[fetch] {} profile={:?} -> status={} final={} len={} (referer={})",
                    page_url, profile, status, final_url, len, referer
                );

                if len > 4000 && text.contains("EntityList-item") {
                    return Ok(text);
                }

                // Not good enough → flip profile and back off
                profile = match profile {
                    Profile::Desktop => Profile::Mobile,
                    Profile::Mobile => Profile::Desktop,
                };
                sleep(Duration::from_millis(rng().random_range(600..1500))).await;
            }
            Err(e) => {
                last_err = Some(e.into());
                sleep(Duration::from_millis(rng().random_range(600..1500))).await;
            }
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow!("failed to fetch page after retries")))
}

// -------------------------
// Parsing helpers
// -------------------------

fn parse_card(
    li: &scraper::ElementRef,
    page_url: &Url,
    body_sel: &Selector,
    title_a: &Selector,
    price_sel: &Selector,
    desc_main: &Selector,
) -> Option<PriceHit> {
    let scope = li.select(body_sel).next().unwrap_or(*li);
    let title = scope
        .select(title_a)
        .next()
        .map(|e| e.text().collect::<String>().trim().to_string())
        .unwrap_or_default();

    let raw_price = scope
        .select(price_sel)
        .next()
        .map(|e| e.text().collect::<String>().trim().to_string())
        .unwrap_or_default();

    let href = scope
        .select(title_a)
        .next()
        .and_then(|a| a.value().attr("href"))
        .map(|s| s.to_string())
        .or_else(|| li.value().attr("data-href").map(|s| s.to_string()));

    let listing_url = href
        .and_then(|h| page_url.join(h.as_str()).ok())
        .map(|u| u.to_string())
        .unwrap_or_default();

    if listing_url.is_empty() || raw_price.is_empty() {
        return None;
    }

    let id = extract_id(&listing_url);
    let (price_numeric, currency) = normalize_price(&raw_price);
    let sqm = extract_sqm_from_li(li, desc_main).or_else(|| extract_sqm_from_li(&scope, desc_main));
    let price_per_m2 = match (price_numeric, sqm) {
        (Some(p), Some(s)) if s > 0.0 => Some(p / s),
        _ => None,
    };

    Some(PriceHit {
        id,
        listing_url,
        title,
        price_numeric,
        currency,
        raw_price,
        sqm,
        price_per_m2,
    })
}

fn extract_id(url: &str) -> String {
    if let Some(pos) = url.rfind("-oglas-") {
        let tail = &url[pos + 7..];
        let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
        return digits;
    }
    url.chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .chars()
        .rev()
        .collect()
}

fn extract_sqm_from_li(node: &scraper::ElementRef, desc_main: &Selector) -> Option<f64> {
    let txt = node
        .select(desc_main)
        .next()
        .map(|n| n.text().collect::<String>())?;
    for token in txt.split(|c: char| c.is_whitespace() || c == ',' || c == ';' || c == '\n') {
        let cleaned = token.replace('.', "").replace(',', ".");
        if let Ok(v) = cleaned.parse::<f64>() {
            return Some(v);
        }
    }
    None
}

fn normalize_price(s: &str) -> (Option<f64>, Option<String>) {
    let mut cur = None;
    if s.contains('€') {
        cur = Some("EUR".to_string());
    } else if s.to_lowercase().contains("kn") {
        cur = Some("HRK".to_string());
    }

    if !s.chars().any(|c| c.is_ascii_digit()) {
        return (None, cur);
    }

    let digits: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_digit() || c == ',' || c == '.' {
                c
            } else {
                ' '
            }
        })
        .collect::<String>()
        .replace('.', "")
        .replace(',', ".");
    let n = digits
        .split_whitespace()
        .find_map(|t| t.parse::<f64>().ok());
    (n, cur)
}

// -------------------------
// Pager helpers (page=N scheme)
// -------------------------

fn normalize_pager(url: &Url) -> (Url, usize) {
    let mut base = url.clone();

    let mut start_page: usize = 1;
    if let Some(q) = base.query() {
        for kv in q.split('&') {
            if let Some(v) = kv.strip_prefix("page=") {
                if let Ok(n) = v.parse::<usize>() {
                    start_page = n.max(1);
                }
            }
        }
    }

    let mut qp: Vec<(String, String)> = vec![];
    for (k, v) in base.query_pairs() {
        if k != "page" {
            qp.push((k.into_owned(), v.into_owned()));
        }
    }
    base.query_pairs_mut()
        .clear()
        .extend_pairs(qp.iter().map(|(k, v)| (&**k, &**v)));

    (base, start_page)
}

fn build_page_url(base: &Url, page: usize) -> Result<Url> {
    let mut u = base.clone();
    let mut qp: Vec<(String, String)> = vec![];
    for (k, v) in u.query_pairs() {
        qp.push((k.into_owned(), v.into_owned()));
    }
    qp.push(("page".to_string(), page.to_string()));
    u.query_pairs_mut()
        .clear()
        .extend_pairs(qp.iter().map(|(k, v)| (&**k, &**v)));
    Ok(u)
}

// -------------------------
// Misc helpers
// -------------------------

fn random_desktop_ua() -> String {
    const UAS: &[&str] = &[
        "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36",
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/123.0 Safari/537.36",
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Safari/605.1.15",
    ];
    let i = rng().random_range(0..UAS.len());
    UAS[i].to_string()
}

fn random_mobile_ua() -> String {
    const UAS: &[&str] = &[
        "Mozilla/5.0 (Linux; Android 14; Pixel 7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Mobile Safari/537.36",
        "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1",
    ];
    let i = rng().random_range(0..UAS.len());
    UAS[i].to_string()
}
