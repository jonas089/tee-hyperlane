// The asset picker the venue uses: every listed token, ours and launched, searchable by
// symbol, name or id.

import { Picker } from "../Picker";
import type { Asset } from "../../trade";
import { shorten } from "../shared";

export function AssetPicker({
  value,
  assets,
  onChange,
}: {
  value: string;
  assets: Asset[];
  onChange: (id: string) => void;
}) {
  const current = assets.find((a) => a.id === value);
  return (
    <Picker
      value={value}
      onChange={onChange}
      placeholder="Search by symbol, name or id"
      className="token-picker"
      options={assets.map((a) => ({
        value: a.id,
        label: a.symbol,
        keywords: `${a.name} ${a.id}`,
        icon: <TokenMark symbol={a.symbol} launched={a.launched} />,
        hint: a.launched ? shorten(a.id.slice(-8), 4) : a.name === a.symbol ? undefined : a.name,
      }))}
      trigger={<span>{current?.symbol ?? "Pick"}</span>}
    />
  );
}

/// A round mark with the symbol's first letters, outlined for a launched token.
export function TokenMark({ symbol, launched, size = 24 }: { symbol: string; launched: boolean; size?: number }) {
  return (
    <span
      className={launched ? "token-mark launched" : "token-mark"}
      style={{ width: size, height: size, fontSize: size * 0.36 }}
      aria-hidden="true"
    >
      {symbol.slice(0, 3)}
    </span>
  );
}
