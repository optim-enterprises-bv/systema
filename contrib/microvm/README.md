# microVM sandbox for swarm agents

Swarm-facing (`public`) agents are denied a host shell by default — a stranger's agent should never
touch your machine. With this sandbox enabled, a public agent can instead run a shell **inside a
libkrun microVM**: a real VM with its own guest kernel, no host filesystem (only the agent's
workspace, mounted at `/work`), and no network. So you can give swarm agents real execution without
real exposure.

If the runtime isn't present, a public shell simply stays denied — enabling the sandbox only ever
*adds* capability, never weakens isolation.

## Requirements (Fedora)
```sh
sudo dnf install libkrun crun-krun libkrunfw     # the microVM runtime
# KVM must be available:
ls -l /dev/kvm
# pull the guest image once (small):
podman pull docker.io/library/alpine:latest
```
Other distros: install `libkrun` + a `krun`-enabled `crun`, and have `/dev/kvm`.

## Enable it
```toml
# ~/.systema/systema.toml
[sandbox]
enabled = true
image = "docker.io/library/alpine:latest"   # the guest; any OCI image works
runtime = "krun"
timeout_secs = 120
```
Then give a public agent a shell (it will run sandboxed):
```toml
# agents/<name>.toml   (public = true)
[capabilities]
shell = true
```
Restart the node. Verify the isolation — a public agent's `shell` should report the **guest** kernel
and an empty `/home`:
```sh
systema ask <public-agent> "run: uname -r; cat /proc/1/comm; ls /home"
# → 6.12.x (libkrunfw guest kernel) · init.krun · (empty) — the host is not visible
```

## How it runs
`podman run --rm --runtime krun --network=none -v <workspace>:/work:Z -w /work <image> sh -lc "<cmd>"`.
Non-public agents are unaffected — they keep their normal host shell. Only public agents are routed
through the VM.

## Notes
- `--network=none` means a sandboxed agent has no outbound path — it can't exfiltrate even what it
  computes. (A future opt-in could allow a filtered network for agents that need it.)
- The workspace mount persists; everything else in the VM is discarded on exit (`--rm`).
- Rootless podman works; the first run per image is slower while the VM image is prepared.
