const { chromium } = require('playwright');
const fs = require('fs'), path = require('path');
(async () => {
  const dir = __dirname, out = path.join(dir, 'screens');
  fs.mkdirSync(out, { recursive: true });
  const canvas = JSON.parse(fs.readFileSync(path.join(dir, 'canvas.json')));
  const browser = await chromium.launch();
  for (const file of canvas.order) {
    const b = canvas.boards[file];
    for (const dark of [0, 1]) {
      const page = await browser.newPage({ viewport: { width: b.w, height: b.h }, deviceScaleFactor: 1 });
      const errs = [];
      page.on('pageerror', e => errs.push(e.message));
      await page.route(/fonts\.(googleapis|gstatic)\.com/, r => r.abort());
      await page.goto('file://' + path.join(dir, file) + '?dark=' + dark);
      await page.waitForTimeout(150);
      const name = file.replace('.dc.html', '') + (dark ? '-dark' : '-light') + '.png';
      await page.screenshot({ path: path.join(out, name) });
      if (errs.length) console.log(file, errs);
      await page.close();
    }
  }
  await browser.close();
})();
