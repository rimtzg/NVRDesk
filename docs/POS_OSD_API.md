# API de OSD Dinámico y Máscaras de Privacidad para POS

NVRDesk expone una API local **HTTP REST** y **WebSocket** para que cualquier software TPV / POS (o middleware de caja) inyecte información en tiempo real sobre el video grabado por el NVR/VMS: número de ticket, cajero activo, artículos escaneados, apertura de cajón, anulaciones o alertas de supervisor.

Cada línea inyectada se renderiza simultáneamente en **MainStream**, **SubStream** y los **snapshots HTTP / MJPEG**, y opcionalmente emite eventos ONVIF `PullPointSubscription` para disparar alertas o marcadores de grabación en el NVR.

---

## URL Base y Requisitos

- **HTTP:** `http://<host>:8080` (puerto configurable vía `--http-port` o `[onvif].http_port`)
- **HTTPS / WSS:** `https://<host>:8080` y `wss://<host>:8080` (cuando `--https` o `[tls].https_enabled = true`)
- **Requisito:** La Web UI / servidor HTTP embebido debe estar habilitado (`daemon.web_ui_enabled = true`, valor por defecto; no uses `--no-web-ui`).

---

## Endpoints HTTP REST

### 1. Inyectar o actualizar texto OSD (`POST /api/v1/osd/text`)

Crea o reemplaza una línea OSD identificada por `slot`. Pueden coexistir múltiples `slot` visibles al mismo tiempo (ordenados alfabéticamente por nombre de `slot`).

- **Método y ruta:** `POST /api/v1/osd/text`
- **Headers:** `Content-Type: application/json`

#### Campos del body JSON

| Campo | Tipo | Obligatorio | Default | Descripción |
| :--- | :--- | :--- | :--- | :--- |
| `text` | `string` | Sí (salvo `action="clear"`) | `""` | Texto a mostrar en pantalla. Si llega vacío (o solo espacios), elimina ese `slot`. |
| `slot` | `string` | No | `"ticket"` | Identificador lógico de la línea (ej. `"cashier"`, `"ticket"`, `"drawer"`). |
| `level` | `"info" \| "warning" \| "alert"` | No | `"info"` | Severidad visual: acento azul (`info`), ámbar (`warning`) o rojo (`alert`). |
| `ttl_ms` | `integer` (`u64`) | No | `pos.default_ttl_ms` (`8000`) | Tiempo de vida en ms antes de desaparecer automáticamente. **`0` = fijo (sticky)** hasta borrarlo. |
| `emit_event` | `boolean` | No | `false` | Si es `true` y `text` no está vacío, publica un evento ONVIF `tns1:RuleEngine/PosEvent/Alert`. |
| `action` | `"clear"` | No | `null` | Si vale `"clear"`, limpia el `slot` indicado (o todos si se omite `slot`). |

#### Respuesta (`200 OK`)

Devuelve un arreglo JSON con las líneas activas actualmente:

```json
[
  {
    "slot": "ticket",
    "text": "Ticket #4821 | Cajero: Ana | Total: $349.50",
    "level": "warning"
  }
]
```

---

### 2. Consultar líneas activas (`GET /api/v1/osd/text`)

Lista las líneas OSD activas no expiradas.

```bash
curl http://127.0.0.1:8080/api/v1/osd/text
```

---

### 3. Limpiar un slot específico (`DELETE /api/v1/osd/text/{slot}`)

Elimina únicamente el `slot` indicado. Devuelve `204 No Content`.

```bash
curl -X DELETE http://127.0.0.1:8080/api/v1/osd/text/drawer
```

---

### 4. Limpiar todas las líneas (`DELETE /api/v1/osd/text`)

Limpia todos los slots activos de inmediato. Devuelve `204 No Content`.

```bash
curl -X DELETE http://127.0.0.1:8080/api/v1/osd/text
```

---

### 5. Consultar eventos recientes (`GET /api/v1/events`)

Devuelve hasta los últimos `25` eventos almacenados en el `EventBroker` (incluyendo `tns1:RuleEngine/PosEvent/Alert` y `tns1:VideoSource/MotionAlarm`).

```json
[
  {
    "id": 1,
    "utc_time": "2026-11-21T01:22:10Z",
    "topic": "tns1:RuleEngine/PosEvent/Alert",
    "source_name": "Slot",
    "source_value": "drawer",
    "data_name": "Message",
    "data_value": "alert: CAJON ABIERTO SIN VENTA"
  }
]
```

---

## API WebSocket (`GET /api/v1/ws/osd`)

Ideal para puntos de venta que actualizan el ticket en cada producto escaneado sin abrir una conexión HTTP por petición.

- **URL:** `ws://127.0.0.1:8080/api/v1/ws/osd` (o `wss://` si `--https` está activo)
- **Mensajes entrantes (cliente -> NVRDesk):** Texto JSON con el mismo esquema de `POST /api/v1/osd/text`.
- **Mensajes salientes (NVRDesk -> cliente):** JSON con el arreglo actualizado de `PosOsdLine` activas.

Ejemplo de payloads por WebSocket:

```json
{"slot":"cashier","text":"Caja 01 | Turno Matutino | Luis M.","level":"info","ttl_ms":0}
```

```json
{"slot":"ticket","text":"SKU 75010553 | Refresco 600ml | $22.00","level":"info","ttl_ms":10000}
```

```json
{"action":"clear","slot":"ticket"}
```

---

## Integración con Eventos ONVIF (`PullPointSubscription`)

Cuando se envía `"emit_event": true` junto con un `text` no vacío:

1. NVRDesk publica el evento en `EventBroker` bajo el tópico ONVIF:
   - **Topic:** `tns1:RuleEngine/PosEvent/Alert`
   - **Source:** `Name="Slot" Value="<slot>"`
   - **Data:** `Name="Message" Value="<level>: <text>"`
2. Cualquier NVR/VMS suscrito a `/onvif/events_service` vía `CreatePullPointSubscription` + `PullMessages` recibe el `<wsnt:NotificationMessage>` en tiempo real.

---

## Configuración `[pos]` y Máscaras de Privacidad Fijas

En `nvrdesk.toml` (o mediante `PUT /api/v1/config` / Web UI):

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

### Reglas de coordenadas en `privacy_masks`
- Todos los valores (`x`, `y`, `width`, `height`) son **normalizados de `0.0` a `1.0`** respecto al tamaño del frame de salida, por lo que cubren exactamente la misma región de pantalla tanto en `MainStream` (1080p), `SubStream` (360p) y snapshots HTTP.
- Las máscaras se dibujan como rectángulos opacos (`#0F172A`, con `label` opcional si el área mide al menos ~2 caracteres x 22 px) **antes** de componer el OSD POS, overlays de teclas/clics y codificar video/JPEG.

---

## Ejemplos de Uso

### Bash (`curl`)

```bash
# 1) Mostrar cajero fijo (ttl_ms = 0)
curl -X POST http://127.0.0.1:8080/api/v1/osd/text \
  -H "Content-Type: application/json" \
  -d '{"slot":"cashier","text":"Caja 02 | Operador: Ana P.","level":"info","ttl_ms":0}'

# 2) Notificar cobro temporal (8 segundos por defecto)
curl -X POST http://127.0.0.1:8080/api/v1/osd/text \
  -H "Content-Type: application/json" \
  -d '{"slot":"ticket","text":"Ticket #4821 | Total: $349.50 | Tarjeta","level":"info"}'

# 3) Alertar apertura manual de cajón y disparar evento ONVIF
curl -X POST http://127.0.0.1:8080/api/v1/osd/text \
  -H "Content-Type: application/json" \
  -d '{"slot":"drawer","text":"APERTURA MANUAL DE CAJON","level":"alert","ttl_ms":12000,"emit_event":true}'
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
        text="Ticket #4822 | Cancelacion parcial -$85.00",
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
      text: "Ticket #9012 | Escaneando articulos...",
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
