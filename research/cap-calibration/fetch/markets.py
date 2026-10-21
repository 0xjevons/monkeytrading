"""Pull public price history for every launch market, plus official series to cross-check them.

- Hourly bars over `HOURLY_START`..`END` (the model's bootstrap window): Binance spot klines for
  crypto, Yahoo Finance chart bars for everything else. Yahoo serves hourly bars for the last 730
  days only, so a later re-run needs a later window; the derived dataset keeps this one.
- Daily bars from `DAILY_START` for the long-history statistics.
- FRED daily series for the cross-checks and benchmark rates in `catalogue`.
- Hyperliquid daily candles over `MIX_START`..`MIX_END` for the markets `catalogue.HYPERLIQUID`
  maps, which set the model's market mix.
- Binance hourly BTC over the papertrade window, for the flow's response to price.

Each response is stored as fetched, gzipped, under `<out>/<source>/`.
"""

import gzip
import json
import time
import urllib.request
from datetime import UTC, datetime
from pathlib import Path

from catalogue import FRED_BENCHMARKS, FRED_CHECKS, HYPERLIQUID, SERIES

HOURLY_START = datetime(2024, 10, 28, tzinfo=UTC)  # a Monday, inside Yahoo's 730-day hourly limit
END = datetime(2026, 10, 17, tzinfo=UTC)  # the Saturday after the last whole week
DAILY_START = datetime(2016, 1, 1, tzinfo=UTC)
MIX_START = datetime(2026, 9, 20, tzinfo=UTC)
MIX_END = datetime(2026, 10, 20, tzinfo=UTC)
PAPERTRADE_START = datetime(2026, 10, 6, tzinfo=UTC)
PAPERTRADE_END = datetime(2026, 10, 21, 6, tzinfo=UTC)
HYPERLIQUID_INFO = "https://api.hyperliquid.xyz/info"
BINANCE = "https://data-api.binance.vision/api/v3/klines"
YAHOO = "https://query1.finance.yahoo.com/v8/finance/chart/"
FRED = "https://fred.stlouisfed.org/graph/fredgraph.csv?id="


def get(url: str, attempts: int = 5) -> bytes:
    for attempt in range(attempts):
        try:
            req = urllib.request.Request(url, headers={"user-agent": "Mozilla/5.0 cap-calibration"})
            with urllib.request.urlopen(req, timeout=60) as resp:
                return resp.read()
        except Exception:
            if attempt == attempts - 1:
                raise
            time.sleep(2 * (attempt + 1))
    raise AssertionError("unreachable")


def save(path: Path, body: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(gzip.compress(body))


def ms(t: datetime) -> int:
    return int(t.timestamp() * 1000)


def binance(out: Path, ticker: str, interval: str, start: datetime, end: datetime = END, suffix: str = "") -> None:
    path = out / "binance" / f"{ticker}_{interval}{suffix}.json.gz"
    if path.exists():
        return
    rows, cursor = [], ms(start)
    while cursor < ms(end):
        page = json.loads(get(f"{BINANCE}?symbol={ticker}&interval={interval}&startTime={cursor}&endTime={ms(end) - 1}&limit=1000"))
        if not page:
            break
        rows += page
        cursor = page[-1][0] + 1
        time.sleep(0.2)
    save(path, json.dumps(rows).encode())


def yahoo(out: Path, ticker: str, interval: str, start: datetime) -> None:
    path = out / "yahoo" / f"{ticker}_{interval}.json.gz"
    if path.exists():
        return
    url = f"{YAHOO}{ticker}?period1={int(start.timestamp())}&period2={int(END.timestamp())}&interval={interval}"
    if interval == "1d":
        url += "&events=div,split"
    body = get(url)
    if not json.loads(body)["chart"]["result"]:
        raise RuntimeError(f"yahoo {ticker} {interval}: no data")
    save(path, body)
    time.sleep(0.5)


def hyperliquid(out: Path, coin: str) -> None:
    path = out / "hyperliquid" / f"{coin.replace(':', '_')}_1d.json.gz"
    if path.exists():
        return
    req = {"type": "candleSnapshot", "req": {"coin": coin, "interval": "1d", "startTime": ms(MIX_START), "endTime": ms(MIX_END) - 1}}
    request = urllib.request.Request(HYPERLIQUID_INFO, json.dumps(req).encode(), {"content-type": "application/json"})
    with urllib.request.urlopen(request, timeout=60) as resp:
        body = resp.read()
    if not isinstance(json.loads(body), list):
        raise RuntimeError(f"hyperliquid {coin}: unexpected reply")
    save(path, body)
    time.sleep(0.3)


def fred(out: Path, series_id: str) -> None:
    path = out / "fred" / f"{series_id}.csv.gz"
    if not path.exists():
        save(path, get(FRED + series_id))


def fetch(out: Path) -> None:
    for symbol, (source, ticker, _) in SERIES.items():
        if source == "binance":
            binance(out, ticker, "1h", HOURLY_START)
            binance(out, ticker, "1d", DAILY_START)
        else:
            yahoo(out, ticker, "1h", HOURLY_START)
            yahoo(out, ticker, "1d", DAILY_START)
        print(f"fetched {symbol}")
    for series_id in [*FRED_CHECKS.values(), *FRED_BENCHMARKS.values()]:
        fred(out, series_id)
    for coin in HYPERLIQUID.values():
        hyperliquid(out, coin)
    binance(out, "BTCUSDT", "1h", PAPERTRADE_START, PAPERTRADE_END, "_papertrade")
    manifest = {
        "hourly_start": HOURLY_START.isoformat(),
        "end": END.isoformat(),
        "daily_start": DAILY_START.isoformat(),
        "fetched_at": datetime.now(UTC).isoformat(timespec="seconds"),
        "mix_window": [MIX_START.isoformat(), MIX_END.isoformat()],
        "sources": {"binance": BINANCE, "yahoo": YAHOO, "fred": FRED, "hyperliquid": HYPERLIQUID_INFO},
    }
    (out / "manifest.json").write_text(json.dumps(manifest, indent=1))
