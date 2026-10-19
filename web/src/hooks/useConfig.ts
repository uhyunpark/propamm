import { useEffect, useState } from "react";
import { type BotConfig, fetchConfig } from "../lib/api";
import { backoffMs } from "../lib/backoff";

/** Loads /config from the bot, retrying with backoff until it answers. */
export function useConfig(): { config: BotConfig | null; online: boolean } {
  const [config, setConfig] = useState<BotConfig | null>(null);
  const [online, setOnline] = useState(false);
  useEffect(() => {
    let alive = true;
    let timer: ReturnType<typeof setTimeout> | undefined;
    let attempt = 0;
    const load = () => {
      fetchConfig()
        .then((c) => {
          if (!alive) return;
          setConfig(c);
          setOnline(true);
        })
        .catch(() => {
          if (!alive) return;
          setOnline(false);
          timer = setTimeout(load, backoffMs(attempt++));
        });
    };
    load();
    return () => {
      alive = false;
      clearTimeout(timer);
    };
  }, []);
  return { config, online };
}
