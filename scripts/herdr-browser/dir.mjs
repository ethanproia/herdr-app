// Where the app keeps browser state: $HERDR_BROWSER_DIR, else $HERDR_APP_HOME/browser (same home rule as src/mods.rs app_dir).
import { join } from "node:path";
import { homedir } from "node:os";

export const browserDir = () => process.env.HERDR_BROWSER_DIR
  || join(process.env.HERDR_APP_HOME || join(process.env.XDG_CONFIG_HOME || join(homedir(), ".config"), "herdr-app"), "browser");
