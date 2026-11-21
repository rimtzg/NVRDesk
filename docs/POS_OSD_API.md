# POS Dynamic OSD & Privacy Masks API

NVRDesk exposes a local **HTTP REST** and **WebSocket** API so any POS / retail checkout application (or middleware) can inject real-time overlay text onto video recorded by the NVR/VMS: ticket numbers, active cashier, scanned items, cash drawer opens, voids, or supervisor alerts.

Each injected line renders simultaneously on **MainStream**, **SubStream**, and **HTTP / MJPEG snapshots**, and can optionally emit ONVIF `PullPointSubscription` events to trigger NVR recording bookmarks or alerts.

---

## Base URL & Prerequisites

- **HTTP:** `http://<host>:8080` (port configurable via `--http-port` or `[onvif].http_port`)
- **HTTPS / WSS:** `https://<host>:8080` and `wss://<host>:8080` (when `--https` or `[tls].https_enabled = true`)
- **Prerequisite:** The embedded Web UI / HTTP server must remain enabled (`daemon.web_ui_enabled = true`, default; do not pass `--no-web-ui`).

---

## HTTP REST Endpoints

### 1. Inject or Update OSD Text (`POST /api/v1/osd/text`)

Creates or replaces an OSD line identified by `slot`. Multiple visible `slot` entries can coexist simultaneously (sorted alphabetically by `slot` name).

- **Method & Path:** `POST /api/v1/osd/text`
- **Headers:** `Content-Type: application/json`

#### JSON Body Fields

| Field | Type | Required | Default | Description |
| :--- | :--- | :--- | :--- | :--- |
| `text` | `string` | Yes (unless `action="clear"`) | `""` | Text displayed on screen. Sending empty text (or whitespace only) clears that `slot`. |
| `slot` | `string` | No | `"ticket"` | Logical identifier for the line (e.g. `"cashier"`, `"ticket"`, `"drawer"`). |
| `level` | `"info" \| "warning" \| "alert"` | No | `"info"` | Visual severity: blue accent (`info`), amber (`warning`), or red (`alert`). |
| `ttl_ms` | `integer` (`u64`) | No | `pos.default_ttl_ms` (`8000`) | Time-to-live in milliseconds before auto-expiring. **`0` = sticky** until explicitly cleared. |
| `emit_event` | `boolean` | No | `false` | When `true` and `text` is non-empty, publishes an ONVIF `tns1:RuleEngine/PosEvent/Alert` event. |
| `action` | `"clear"` | No | `null` | When `"clear"`, clears the specified `slot` (or all slots if `slot` is omitted). |

#### Response (`200 OK`)

Returns a JSON array with currently active OSD lines:

```json
[
  {
    "slot": "ticket",
    "text": "Ticket #4821 | Cashier: Ana | Total: $349.50",
    "level": "warning"
  }
]
```

---

### 2. List Active OSD Lines (`GET /api/v1/osd/text`)

Returns all non-expired active OSD lines.

```bash
curl http://127.0.0.1:8080/api/v1/osd/text
```

---

### 3. Clear a Specific Slot (`DELETE /api/v1/osd/text/{slot}`)

Removes only the specified `slot`. Returns `204 No Content`.

```bash
curl -X DELETE http://127.0.0.1:8080/api/v1/osd/text/drawer
```

---

### 4. Clear All Lines (`DELETE /api/v1/osd/text`)

Immediately clears all active OSD slots. Returns `204 No Content`.

```bash
curl -X DELETE http://127.0.0.1:8080/api/v1/osd/text
```

---

### 5. Query Recent Events (`GET /api/v1/events`)

Returns up to the latest `25` events recorded in `EventBroker` (including `tns1:RuleEngine/PosEvent/Alert` and `tns1:VideoSource/MotionAlarm`).

```json
[
  {
    "id": 1,
    "utc_time": "2026-11-21T01:22:10Z",
    "topic": "tns1:RuleEngine/PosEvent/Alert",
    "source_name": "Slot",
    "source_value": "drawer",
    "data_name": "Message",
    "data_value": "alert: MANUAL DRAWER OPEN WITHOUT SALE"
  }
]
```

---

## WebSocket API (`GET /api/v1/ws/osd`)

Ideal for POS software updating the ticket on every scanned barcode without opening a new HTTP connection per item.

- **URL:** `ws://127.0.0.1:8080/api/v1/ws/osd` (or `wss://` when `--https` is active)
- **Incoming messages (client -> NVRDesk):** JSON text following the same schema as `POST /api/v1/osd/text`.
- **Outgoing messages (NVRDesk -> client):** JSON array with updated active `PosOsdLine` entries.

Example WebSocket payloads:

```json
{"slot":"cashier","text":"POS 01 | Morning Shift | Luis M.","level":"info","ttl_ms":0}
```

```json
{"slot":"ticket","text":"SKU 75010553 | Soda 600ml | $22.00","level":"info","ttl_ms":10000}
```

```json
{"action":"clear","slot":"ticket"}
```

---

## ONVIF Event Integration (`PullPointSubscription`)

When `"emit_event": true` is sent alongside non-empty `text`:

1. NVRDesk publishes the event into `EventBroker` under the ONVIF topic:
   - **Topic:** `tns1:RuleEngine/PosEvent/Alert`
   - **Source:** `Name="Slot" Value="<slot>"`
   - **Data:** `Name="Message" Value="<level>: <text>"`
2. Any NVR/VMS subscribed to `/onvif/events_service` via `CreatePullPointSubscription` + `PullMessages` receives the `<wsnt:NotificationMessage>` in real time.

---

## `[pos]` Configuration & Static Privacy Masks

In `nvrdesk.toml` (or via `PUT /api/v1/config` / Web UI interactive Privacy Masks editor):

```toml
[pos]
osd_enabled = true
default_ttl_ms = 8000
osd_position = "top_left" # top_left | top_right | bottom_left | bottom_right

[[pos.privacy_masks]]
x = 0.70
y = 0.78
width = 0.28
height = 0.16
label = "PIN PAD"
```

### Coordinate Rules for `privacy_masks`
- All coordinates (`x`, `y`, `width`, `height`) are **normalized from `0.0` to `1.0`** relative to the output frame dimensions, so they cover the exact same screen region across `MainStream` (1080p), `SubStream` (360p), and HTTP snapshots.
- Masks are drawn as opaque dark rectangles (`#0F172A`, with optional `label` if the box is at least ~2 characters x 22 px) **before** composing POS OSD, keystroke/click overlays, and encoding video or JPEG snapshots.

---

## Code Examples

### Bash (`curl`)

```bash
# 1) Display sticky cashier banner (ttl_ms = 0)
curl -X POST http://127.0.0.1:8080/api/v1/osd/text \
  -H "Content-Type: application/json" \
  -d '{"slot":"cashier","text":"Register 02 | Operator: Ana P.","level":"info","ttl_ms":0}'

# 2) Post temporary checkout line (default 8 seconds)
curl -X POST http://127.0.0.1:8080/api/v1/osd/text \
  -H "Content-Type: application/json" \
  -d '{"slot":"ticket","text":"Ticket #4821 | Total: $349.50 | Card","level":"info"}'

# 3) Alert manual cash drawer open and emit ONVIF event
curl -X POST http://127.0.0.1:8080/api/v1/osd/text \
  -H "Content-Type: application/json" \
  -d '{"slot":"drawer","text":"MANUAL DRAWER OPEN","level":"alert","ttl_ms":12000,"emit_event":true}'
```

### Python (`requests`)

```python
import json

import requests


def send_pos_osd(
    base_url: str,
    slot: str,
    text: str,
    level: str = "info",
    ttl_ms: int = 8000,
    emit_event: bool = False,
) -> list:
    try:
        payload = {
            "slot": slot,
            "text": text,
            "level": level,
            "ttl_ms": ttl_ms,
            "emit_event": emit_event,
        }
        response = requests.post(
            f"{base_url}/api/v1/osd/text",
            headers={"Content-Type": "application/json"},
            data=json.dumps(payload),
            timeout=2.0,
        )
        response.raise_for_status()
        return response.json()
    except requests.RequestException as err:
        print(f"Error sending POS OSD: {err}")
        return []


if __name__ == "__main__":
    send_pos_osd(
        "http://127.0.0.1:8080",
        slot="ticket",
        text="Ticket #4822 | Partial void -$85.00",
        level="warning",
        ttl_ms=10000,
        emit_event=True,
    )
```

### JavaScript / Node.js (`fetch` + `WebSocket`)

```javascript
async function sendPosOsd(baseUrl, payload) {
  const res = await fetch(`${baseUrl}/api/v1/osd/text`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(payload),
  });
  if (!res.ok) {
    throw new Error(`HTTP ${res.status}`);
  }
  return res.json();
}

function connectPosOsdSocket(wsUrl) {
  const ws = new WebSocket(wsUrl);
  ws.addEventListener("open", () => {
    ws.send(JSON.stringify({
      slot: "ticket",
      text: "Ticket #9012 | Scanning items...",
      level: "info",
      ttl_ms: 8000,
    }));
  });
  ws.addEventListener("message", (event) => {
    console.log("Active OSD lines:", JSON.parse(event.data));
  });
  return ws;
}
```
