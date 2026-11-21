document.getElementById("copy-snap-btn").addEventListener("click", async (e) => {
  const btn = e.currentTarget;
  try {
    await navigator.clipboard.writeText("sudo snap install nvrdesk --edge");
    btn.textContent = "Copied!";
    setTimeout(() => { btn.textContent = "Copy"; }, 1800);
  } catch (_) {
    btn.textContent = "Copy";
  }
});

async function loadLatestRelease() {
  try {
    const res = await fetch("https://api.github.com/repos/rimtzg/NVRDesk/releases/latest");
    if (!res.ok) return;
    const rel = await res.json();
    if (rel.tag_name) {
      document.getElementById("release-pill").textContent = `Latest Release: ${rel.tag_name} · MIT License`;
    }
    const assets = rel.assets || [];
    const findAsset = (ext) => assets.find((a) => a.name && a.name.endsWith(ext));

    const winMsi = findAsset(".msi");
    if (winMsi) document.getElementById("dl-win").href = winMsi.browser_download_url;

    const linDeb = findAsset(".deb");
    if (linDeb) document.getElementById("dl-deb").href = linDeb.browser_download_url;

    const macDmg = findAsset(".dmg");
    if (macDmg) document.getElementById("dl-mac").href = macDmg.browser_download_url;
  } catch (_) {
    // Keep fallback links pointing to /releases/latest on offline or rate-limited clients.
  }
}

loadLatestRelease();
