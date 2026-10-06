// State that outlives the view showing it: a trade or a launch keeps running when the page
// switches tab, and the view picks it up again when it comes back.

import { useEffect, useState } from "react";

export function createStore<T>(initial: T) {
  let value = initial;
  const listeners = new Set<(v: T) => void>();
  return {
    get: () => value,
    set(next: T) {
      value = next;
      for (const l of listeners) l(next);
    },
    use(): T {
      const [v, setV] = useState(value);
      useEffect(() => {
        listeners.add(setV);
        setV(value);
        return () => {
          listeners.delete(setV);
        };
      }, []);
      return v;
    },
  };
}
