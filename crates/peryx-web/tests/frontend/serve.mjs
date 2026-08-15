import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { startPeryx } from "./server.mjs";

const here = dirname(fileURLToPath(import.meta.url));
await startPeryx({
  configText: "",
  port: Number(process.env.PERYX_FRONTEND_PORT ?? 4455),
  readyPort: Number(process.env.PERYX_READY_PORT ?? 5455),
  repo: join(here, "..", "..", "..", ".."),
});
