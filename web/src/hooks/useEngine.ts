import { useEffect, useState } from "react";
import { type Engine, type EngineKind, loadEngine } from "../engine/engine";

export function useEngine(): { engine: Engine; kind: EngineKind } | null {
  const [state, setState] = useState<{ engine: Engine; kind: EngineKind } | null>(null);
  useEffect(() => {
    let alive = true;
    loadEngine().then((s) => {
      if (alive) setState(s);
    });
    return () => {
      alive = false;
    };
  }, []);
  return state;
}
