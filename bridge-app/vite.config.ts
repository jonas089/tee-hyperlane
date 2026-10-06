import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// `BRIDGE_PROXY=http://<host>:3000 npm run dev` serves the UI locally against a live gateway,
// forwarding its paths so the browser sees one origin and no CORS rules apply.
// `TRADE_PROXY=http://localhost:3101` sends the trade API alone somewhere else, for working on it.
const gateway = process.env.BRIDGE_PROXY;
const trade = process.env.TRADE_PROXY;
const proxy = gateway
  ? {
      ...(trade ? { "/api/v1/trade": { target: trade, changeOrigin: true } } : {}),
      ...Object.fromEntries(
        ["/rpc", "/rest", "/api", "/evm", "/tx"].map((path) => [path, { target: gateway, changeOrigin: true }]),
      ),
    }
  : undefined;

export default defineConfig({
  plugins: [react()],
  server: { host: true, proxy },
});
