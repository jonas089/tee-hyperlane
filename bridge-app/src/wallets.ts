// MetaMask for the EVM chains, Keplr for Celestia. Nothing else is needed: the bridge never
// asks a wallet to do anything beyond signing a transfer on the chain it already knows.

import type { Chain, CosmosChain, EvmChain } from "./config";

declare global {
  interface Window {
    ethereum?: {
      request(args: { method: string; params?: unknown[] }): Promise<any>;
      on?(event: string, handler: (...args: any[]) => void): void;
    };
    keplr?: any;
    getOfflineSigner?: (chainId: string) => any;
  }
}

export interface Account {
  address: string;
  chain: Chain;
}

export async function connectMetaMask(chain: EvmChain): Promise<Account> {
  if (!window.ethereum) throw new Error("MetaMask is not installed");
  const [address] = await window.ethereum.request({ method: "eth_requestAccounts" });
  await switchEvmChain(chain);
  return { address, chain };
}

/** Switch, adding the network first if MetaMask does not know it (error 4902). */
export async function switchEvmChain(chain: EvmChain): Promise<void> {
  if (!window.ethereum) throw new Error("MetaMask is not installed");
  try {
    await window.ethereum.request({
      method: "wallet_switchEthereumChain",
      params: [{ chainId: chain.chainIdHex }],
    });
  } catch (error: any) {
    if (error?.code !== 4902) throw error;
    await window.ethereum.request({
      method: "wallet_addEthereumChain",
      params: [
        {
          chainId: chain.chainIdHex,
          chainName: chain.name,
          nativeCurrency: { name: "Ether", symbol: "ETH", decimals: 18 },
          rpcUrls: [chain.rpc],
          blockExplorerUrls: [chain.explorer],
        },
      ],
    });
  }
}

export async function connectKeplr(chain: CosmosChain): Promise<Account> {
  if (!window.keplr) throw new Error("Keplr is not installed");
  try {
    await window.keplr.enable(chain.chainId);
  } catch {
    // Keplr does not ship mocha-5 by default.
    await window.keplr.experimentalSuggestChain(suggestChain(chain));
    await window.keplr.enable(chain.chainId);
  }
  const signer = window.getOfflineSigner!(chain.chainId);
  const [account] = await signer.getAccounts();
  return { address: account.address, chain };
}

function suggestChain(chain: CosmosChain) {
  const currency = { coinDenom: "TIA", coinMinimalDenom: chain.denom, coinDecimals: 6 };
  return {
    chainId: chain.chainId,
    chainName: chain.name,
    rpc: chain.rpc,
    rest: chain.rest,
    bip44: { coinType: 118 },
    bech32Config: {
      bech32PrefixAccAddr: chain.bech32Prefix,
      bech32PrefixAccPub: `${chain.bech32Prefix}pub`,
      bech32PrefixValAddr: `${chain.bech32Prefix}valoper`,
      bech32PrefixValPub: `${chain.bech32Prefix}valoperpub`,
      bech32PrefixConsAddr: `${chain.bech32Prefix}valcons`,
      bech32PrefixConsPub: `${chain.bech32Prefix}valconspub`,
    },
    currencies: [currency],
    feeCurrencies: [{ ...currency, gasPriceStep: { low: 0.01, average: 0.02, high: 0.1 } }],
    stakeCurrency: currency,
  };
}

export function walletFor(chain: Chain): "MetaMask" | "Keplr" {
  return chain.kind === "evm" ? "MetaMask" : "Keplr";
}
