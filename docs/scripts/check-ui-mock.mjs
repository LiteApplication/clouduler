import {existsSync} from 'node:fs';
if (!existsSync(new URL('../static/ui-mock/mock.html', import.meta.url))) {
  console.warn('\n[docs] static/ui-mock/mock.html is missing: <UiScreen> will show placeholders.\n[docs] Run `npm run build:ui-mock` (or `npm run build:full`) for real screens.\n');
}
