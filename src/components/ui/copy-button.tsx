import * as React from "react";
import { Check, Copy } from "lucide-react";

import { Button } from "@/components/ui/button";

/**
 * 一键复制按钮：点击后把 text 写入剪贴板，1.5s 内图标变为对勾。
 * 从 GlmProxyPage 提取为共享组件，供各网关页的「接入信息」区复用。
 */
export function CopyButton({ text, label }: { text: string; label: string }) {
  const [copied, setCopied] = React.useState(false);
  return (
    <Button
      size="icon"
      variant="ghost"
      className="size-7"
      aria-label={`复制${label}`}
      onClick={() => {
        void navigator.clipboard.writeText(text).then(
          () => {
            setCopied(true);
            window.setTimeout(() => setCopied(false), 1500);
          },
          () => undefined,
        );
      }}
    >
      {copied ? <Check className="size-3.5" /> : <Copy className="size-3.5" />}
    </Button>
  );
}
