// Docs (Agents): how an agent connects to the venue, by MCP or straight over HTTP.

import { Copy } from "../shared";
import { RELAYER_API } from "../../config";

const SECTIONS = [
  ["start", "Connect an agent"],
  ["tools", "MCP tools"],
  ["http", "HTTP API"],
  ["trade", "Trading"],
  ["launch", "Launching a token"],
  ["liquidity", "Liquidity"],
  ["trust", "What secures it"],
] as const;

export function DocsView() {
  const origin = typeof window === "undefined" ? "" : window.location.origin;
  const api = RELAYER_API.startsWith("http") ? RELAYER_API : `${origin}${RELAYER_API}`;
  const mcpConfig = `{
  "mcpServers": {
    "teeism": {
      "command": "node",
      "args": ["/path/to/tee-hyperlane/mcp/server.mjs"],
      "env": {
        "TEEISM_URL": "${origin}",
        "EVM_PRIVATE_KEY": "0x…",
        "CELESTIA_MNEMONIC": "word word …"
      }
    }
  }
}`;
  const local = `git clone https://github.com/jonas089/tee-hyperlane
cd tee-hyperlane/mcp && npm install
claude mcp add teeism -e TEEISM_URL=${origin} \\
  -e EVM_PRIVATE_KEY=0x… -e CELESTIA_MNEMONIC="word word …" \\
  -- node $PWD/server.mjs`;

  return (
    <div className="docs">
      <aside className="docs-toc">
        <nav aria-label="On this page">
          {SECTIONS.map(([id, label]) => (
            <a key={id} href={`#trade/docs`} onClick={(e) => { e.preventDefault(); document.getElementById(`docs-${id}`)?.scrollIntoView({ behavior: "smooth" }); }}>
              {label}
            </a>
          ))}
        </nav>
      </aside>

      <article className="docs-body">
        <header>
          <h1>Bring your agent</h1>
          <p className="lead">
            Agents trade, launch tokens and provide liquidity here the same way people do: across Celestia, Ethereum
            Sepolia, Base Sepolia and Arbitrum Sepolia, with every transfer verified by an enclave instead of a
            validator set.
          </p>
        </header>

        <section id="docs-start">
          <h2>Connect an agent</h2>
          <p>
            The quickest way is the MCP server. It gives any MCP client, such as Claude, tools to quote, trade, launch
            and add liquidity. With keys it signs and sends; without them it quotes and returns unsigned
            transactions.
          </p>
          <ol className="docs-steps">
            <li>
              <strong>Get the server</strong>, a small Node program in this repository, and add it to Claude Code:
              <Code text={local} />
              Any other MCP client takes the same in its config:
              <Code text={mcpConfig} />
            </li>
            <li>
              <strong>Fund its accounts.</strong> An EVM account with a little ETH on each chain it uses, and a Celestia
              account. The <a href="#faucet">Faucet</a> gives every Celestia address TIA and teeUSD once.
            </li>
            <li>
              <strong>Ask.</strong> "What can I trade?", "Buy 50 teeUSD worth of TIA on Base", "Launch MOON with a
              1B supply at 0.001 teeUSD and 5% pools on Base and Arbitrum".
            </li>
          </ol>
          <p className="note">
            Use keys made for the agent and fund them with what you are willing to let it spend. They stay in the MCP
            process on your machine and are never sent anywhere.
          </p>
        </section>

        <section id="docs-tools">
          <h2>MCP tools</h2>
          <dl className="docs-defs">
            <dt>venue_info</dt>
            <dd>Every listed asset with its id, chains and pools. Call it first.</dd>
            <dt>quote</dt>
            <dd>The price and steps for a trade. Amounts are whole units, such as 12.5.</dd>
            <dt>balances</dt>
            <dd>What an account holds of every asset, per chain.</dd>
            <dt>trade</dt>
            <dd>Runs a quoted route with the server's keys. Returns a job id.</dd>
            <dt>launch</dt>
            <dd>Launches a token and seeds its pools. Returns a job id.</dd>
            <dt>add_liquidity</dt>
            <dd>Adds to a pool, or creates it.</dd>
            <dt>job_status</dt>
            <dd>Progress of a trade or launch. Trades take one to fifteen minutes.</dd>
            <dt>build_step</dt>
            <dd>Unsigned transactions for one step, for an agent that signs elsewhere.</dd>
          </dl>
        </section>

        <section id="docs-http">
          <h2>HTTP API</h2>
          <p>
            Everything the MCP server does goes through these endpoints, under <code>{api}/v1/trade</code>. Amounts are
            base units as strings; every asset has 6 decimals. Nothing here signs: each endpoint returns unsigned
            transactions to send in order.
          </p>
          <table className="docs-table">
            <tbody>
              <Row method="GET" path="/v1/trade" what="Chains, pools, and every listed asset. ?refresh=true re-reads now." />
              <Row method="GET" path="/v1/trade/quote" what="?from&sell&to&buy&amount: the price and the steps." />
              <Row method="POST" path="/v1/trade/build" what="One step to transactions, for the amount actually held." />
              <Row method="POST" path="/v1/trade/pool" what="Add full-range liquidity, creating the pool if needed." />
              <Row method="POST" path="/v1/trade/launch/create" what="Launch 1 of 3: create the token on Celestia." />
              <Row method="POST" path="/v1/trade/launch/deploy" what="Launch 2 of 3: one router per chain." />
              <Row method="POST" path="/v1/trade/launch/wire" what="Launch 3 of 3: mint, connect, renounce." />
              <Row method="GET" path="/v1/trade/launch/{id}" what="A launch's routers, and its listing once wired." />
              <Row method="GET" path="/v1/messages/{id}" what="A transfer's status: pending, verified, delivered." />
            </tbody>
          </table>
          <p>
            The full schema is at <a href={`${api}/v1/openapi.json`}>{api}/v1/openapi.json</a>.
          </p>
          <Code
            text={`curl '${api}/v1/trade/quote?from=celestia&sell=TIA&to=base&buy=teeUSD&amount=10000000'`}
          />
          <p>
            A transaction is either <code>evm</code>, with <code>chainId</code>, <code>to</code>, <code>data</code> and{" "}
            <code>value</code> to send as they are, or <code>cosmos</code>, with <code>msgs</code> to put in one Celestia
            transaction in order. Each message is <code>typeUrl</code> plus its fields in proto JSON names.
          </p>
        </section>

        <section id="docs-trade">
          <h2>Trading</h2>
          <p>
            A quote is a list of steps. A <strong>swap</strong> runs on one chain, through the direct pool for the pair
            or through teeUSD, whichever pays more. A <strong>bridge</strong> moves one asset between Celestia and an
            EVM chain, one for one. Every route goes through Celestia, so moving between two EVM chains is two bridges.
          </p>
          <ol className="docs-steps">
            <li>Quote, then take the steps in order.</li>
            <li>
              For each step, <code>POST /build</code> with the step, the amount you now hold for it, your sender and, for a
              bridge, the recipient on the far side.
            </li>
            <li>Send the transactions. An approval, when needed, comes before its swap.</li>
            <li>
              After a bridge, wait for <code>GET /v1/messages/{"{id}"}</code> to report <code>delivered</code>. The id is
              the Hyperlane message id the origin emitted.
            </li>
          </ol>
          <p className="note">
            A swap reverts rather than fill more than 0.5% below its quote; set <code>slippageBps</code> to change that.
            If a trade stops between steps, the funds are in your own account on that step's chain.
          </p>
        </section>

        <section id="docs-launch">
          <h2>Launching a token</h2>
          <p>
            Anyone can launch. The launcher pays for the deployments and the delivery fees; the relayer pays the gas to
            deliver transfers, as for every other asset.
          </p>
          <ol className="docs-steps">
            <li>
              <code>launch/create</code> with your Celestia account. The transaction's{" "}
              <code>EventCreateSyntheticToken</code> carries the token id.
            </li>
            <li>
              <code>launch/deploy</code> with the id, a name and a symbol: one transaction per chain, each a call to that
              chain's token factory.
            </li>
            <li>
              Wait until <code>GET launch/{"{id}"}</code> lists your routers, then <code>launch/wire</code> with the whole
              supply and your EVM account. One Celestia transaction mints the supply to you, connects the routers, points
              the token at the TEE ISMs and renounces ownership.
            </li>
            <li>
              Within half a minute <code>listed</code> is set and the token trades. Bridge some of it, with teeUSD, to
              each chain and open its pools with <code>/pool</code>.
            </li>
          </ol>
          <p className="note">
            The venue lists a token only once Celestia shows it with no owner, on the routing ISM, and with every router
            one a factory made. Nobody can mint more of it afterwards, its launcher and this venue included.
          </p>
        </section>

        <section id="docs-liquidity">
          <h2>Liquidity</h2>
          <p>
            Pools are Uniswap v3 at the 0.3% tier, across the full price range. <code>/pool</code> adds to an existing pool
            at its price, or creates one at the price your two amounts set. Anyone can add to any pool, and the position is
            an NFT in the sender's wallet, withdrawable in the Uniswap interface or by calling the position manager.
          </p>
        </section>

        <section id="docs-trust">
          <h2>What secures it</h2>
          <p>
            Every transfer between chains is authorised by a light client running in an Intel TDX enclave. The destination
            verifies the enclave's quote itself, so there is no validator set or multisig to trust. Swaps run on the
            chains' own Uniswap deployments. This is a testnet venue; the assets have no value.
          </p>
        </section>
      </article>
    </div>
  );
}

function Code({ text }: { text: string }) {
  return (
    <div className="docs-code">
      <pre>{text}</pre>
      <Copy value={text}>
        <code>Copy</code>
      </Copy>
    </div>
  );
}

function Row({ method, path, what }: { method: string; path: string; what: string }) {
  return (
    <tr>
      <td>
        <span className={`verb verb-${method.toLowerCase()}`}>{method}</span>
      </td>
      <td>
        <code>{path}</code>
      </td>
      <td>{what}</td>
    </tr>
  );
}
