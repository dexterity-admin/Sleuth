# Sleuth office server

`sleuth-server.exe` is an optional server for the office. It runs the same
reading and search engine as each desktop, but over the project shares on the
file server, once for everybody. Desktops ask it for "office" results next to
their own.

It is optional. A desktop with no server set up works exactly as before. If the
server is down, desktops show office results as unavailable and local search
carries on.

## What it does

- Walks the project folders listed in `server.json` and reads every PDF in them
  (text layer, then OCR where needed). If a shared reading cache is set, it
  shares results with the desktops through it.
- Answers desktops over HTTPS on port 8455, API version 1 (`/api/v1`).
- Each person has an access key. The server stores only a SHA-256 hash of the
  key, never the key itself.
- Each person sees only their projects' folders. The server checks this on
  every request, including every document id. A path inside `\\srv\proj`
  never matches `\\srv\project2`.
- Keeps an audit log (`audit.db`). It records refused requests, administrator
  actions, and the changes made with the command line. It never records
  access keys. Search text is cut to 200 characters. The log keeps the newest
  100,000 rows.
- Each person can run 4 searches or page images at the same time, and get 120
  page images a minute. Requests beyond that are answered with
  `429 rate_limited`.

## Files

Everything lives in the data folder (default `C:\ProgramData\Sleuth Server`;
choose another with `--data "<folder>"` on every command):

| File | What it is |
|---|---|
| `server.json` | Listen address, projects, users (key hashes), shared cache folder, reader settings |
| `ca.pem`, `ca.key` | The server's own certificate authority (10 years). Keep `ca.key` safe. |
| `server.pem`, `server.key` | The HTTPS certificate (2 years), signed by the CA |
| `audit.db` | Audit log |
| `data\index.db` | The index (the engine's data folder is `data\`) |
| `data\models\`, `config\` | Engine downloads and settings, made by the server |
| `logs\server.log` | Service log (the console shows the log when you use `run`) |

## Installing (IT)

Run these in an administrator command prompt in the folder that holds
`sleuth-server.exe`. Keep `pdfium.dll` and the `models` folder next to the
exe, as in the desktop install.

1. **Set up** the data folder, the certificates and the first projects. Use UNC
   paths. A Windows service cannot see mapped drive letters.

   ```
   sleuth-server init --project "P12513=\\fileserver\Projects\P12513" --project "P13001=\\fileserver\Projects\P13001,\\fileserver\Archive\P13001" --shared-cache "\\fileserver\Sleuth-cache"
   ```

   This prints the names and addresses in the certificate and the CA
   fingerprint (SHA-256). It also copies the CA certificate to
   `\\fileserver\Sleuth-cache\sleuth-server-ca.pem`. Desktops look for it
   there. Add more DNS names with `--hostname sleuth.office.local`.

2. **Add more projects** later:

   ```
   sleuth-server add-project "P14002=\\fileserver\Projects\P14002"
   sleuth-server remove-project P14002
   ```

3. **Add people.** Each key is printed once. Send it to the person privately.

   ```
   sleuth-server add-user anita --projects P12513,P13001
   sleuth-server add-user it-admin --role admin --projects *
   sleuth-server list-users
   sleuth-server remove-user anita
   ```

   A lost key cannot be recovered. Remove the user and add them again to make
   a new key. A running server applies these changes within about 5 seconds.

4. **Install the service:**

   ```
   sleuth-server service install
   sleuth-server service start
   ```

   The service is called `SleuthServer` and starts automatically. Installing
   it also restricts the data folder to Administrators, SYSTEM and the
   service, because the folder holds the CA key and the key hashes.

5. **Open the firewall** for incoming TCP 8455 (the office network only):

   ```
   netsh advfirewall firewall add rule name="Sleuth server" dir=in action=allow protocol=TCP localport=8455 profile=domain
   ```

6. **Check** the setup:

   ```
   sleuth-server check
   ```

   This checks `server.json`, that each project folder can be listed by the
   account running the command, and the certificate dates.

To try the server in a console window first, use `sleuth-server run`.
Ctrl+C stops it.

## Service account and share access

By default the service runs as the virtual account `NT SERVICE\SleuthServer`.
This is the least-privilege choice: there is no password to manage, and the
account has no rights except the ones you grant. On the network, a virtual
account uses the server's computer account (`DOMAIN\SERVERNAME$`). You can
either:

- give `DOMAIN\SERVERNAME$` **read** access (share and NTFS) to each project
  share, or
- run the service as a domain service account, which is better when several
  servers or an audit trail on the file server are involved. A group Managed
  Service Account (gMSA) is best because Windows rotates its password. Make
  the account, give it **read** access to the project shares and
  **modify** access to the data folder, then:

  ```
  sc.exe config SleuthServer obj= "DOMAIN\svc-sleuth$"
  sc.exe config SleuthServer obj= "DOMAIN\svc-sleuth" password= "..."   (ordinary account instead of a gMSA)
  icacls "C:\ProgramData\Sleuth Server" /grant "DOMAIN\svc-sleuth:(OI)(CI)M"
  sleuth-server service stop
  sleuth-server service start
  ```

  You can also change the account in services.msc > Sleuth office server >
  Log On.

Give read access only. The server never writes to the project shares. It
writes only to the shared reading cache, if one is set, so that account also
needs modify access to the cache folder.

## Connecting a desktop

In Sleuth: **Settings > Office server**

- Address: `https://<server>:8455`. Use a name or address that appears in the
  certificate; `init` and `renew-cert` print them.
- Access key: the key from `add-user`. It is stored encrypted for that
  Windows user.
- Certificate: if the shared cache folder is set, Sleuth finds
  `sleuth-server-ca.pem` there by itself. Otherwise, pick the `ca.pem` copied
  from the server.

Sleuth shows the user, their projects, and whether the server is still on its
first reading pass ("syncing").

## When the server is offline

Every call has a short timeout. If the server cannot be reached, desktops say
"Office results unavailable" and show this PC's results. Nothing else stops,
and they try again every minute.

## Certificates

- The server certificate lasts 2 years. `sleuth-server check` warns 30 days
  before it expires, and the server logs a warning at startup. Renew it with
  `sleuth-server renew-cert`, then restart the service. The CA stays the
  same, so desktops need nothing new.
- Also run `renew-cert` after renaming the server, changing its addresses, or
  adding `hostnames` to `server.json`.
- The CA lasts 10 years. If `ca.key` is lost, restore it from the backup. A
  new CA would mean copying a new `sleuth-server-ca.pem` for every desktop.
  `init` never replaces an existing CA.

## Backups

Copy these from the data folder: `server.json`, `audit.db`, `ca.pem`,
`ca.key`, `server.pem`, `server.key` and `data\index.db`. The index can be
rebuilt by reading everything again, but that takes hours to days.

For a consistent copy, stop the service first (`sleuth-server service stop`),
copy the files, then start it again.

**Restore:** stop the service, copy the files back into the data folder
(`index.db` into `data\`), and start the service.

## Upgrading and rolling back

- **Upgrade:** stop the service, replace `sleuth-server.exe` (with
  `pdfium.dll` and `models`), and start the service. The index format only
  gains columns, so an older server can still open it. The API stays at
  version 1 for compatible changes. A desktop that meets a server with a
  different major version says so and does not use it.
- **Rollback:** stop the service, put the previous exe back, and start the
  service. Keep the data folder as it is.
- Moving to a new machine: install the exe, copy the whole data folder, run
  `sleuth-server renew-cert` (the name and addresses change), then
  `service install` and `service start`, and point desktops at the new
  address.

## API summary (v1)

All under `https://<server>:8455/api/v1`, JSON, `Authorization: Bearer <key>`
except `/health`. Errors are `{"code": ..., "message": ...}` with code
`unauthorized`, `forbidden`, `not_found`, `bad_request`, `rate_limited` or
`unavailable`.

| Method | Path | Notes |
|---|---|---|
| GET | `/health` | `{status, version}`, no key |
| GET | `/capabilities` | api version, user, role, projects, folders, syncing |
| GET | `/projects` | the user's projects |
| GET | `/search?q&project&limit&offset` | `q` up to 500 characters, `limit` up to 200 |
| GET | `/documents?path=` | id of an indexed file the user may see |
| GET | `/documents/{id}` | metadata, pages, provenance |
| GET | `/documents/{id}/pages/{n}?q&tag&max_px` | PNG page with marks; `n` from 1, `max_px` 400 to 3000 |
| GET | `/documents/{id}/tags` | every tag reading in the document |
| GET | `/documents/{id}/related` | documents sharing tags (the user's folders only) |
| GET | `/tags?category&contains&project&limit&offset` | `{tags, total}` |
| GET | `/tags/{norm}/occurrences?project` | |
| GET | `/revisions?contains&project&limit` | |
| GET | `/compare?old={id}&new={id}` | |
| GET | `/jobs?state=waiting\|failed` | reading queue (the user's folders only) |
| POST | `/indexing/scans` | admin: walk all folders now |
| POST | `/jobs/{id}/cancel` | admin |
| GET | `/monitor` | admin |
| GET | `/audit?limit` | admin |
