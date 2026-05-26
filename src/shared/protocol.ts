export type BridgeAction =
  | "tabs.list"
  | "tabs.open"
  | "tabs.close"
  | "tabs.navigate"
  | "tabs.activate"
  | "dom.getHtml"
  | "dom.getText"
  | "dom.contentSummary"
  | "dom.querySelector"
  | "interaction.click"
  | "interaction.type"
  | "interaction.typeSecret"
  | "interaction.scroll"
  | "interaction.pressKey"
  | "interaction.hover"
  | "interaction.mouseMove"
  | "capture.screenshot"
  | "capture.computedStyles"
  | "execution.executeJs"
  | "wait.selector"
  | "wait.navigation"
  | "tabs.goBack"
  | "tabs.goForward"
  | "tabs.reload"
  | "interaction.selectOption"
  | "interaction.check"
  | "dom.formValues"
  | "dialog.setBehavior"
  | "dialog.getLast"
  | "dom.accessibilityTree"
  | "capture.annotate"
  | "capture.clearAnnotations"
  | "interaction.clickAnnotation"
  | "interaction.typeAnnotation"
  | "cookies.get"
  | "cookies.set"
  | "cookies.delete"
  | "storage.get"
  | "storage.set"
  | "storage.clear"
  | "monitor.consoleLogs"
  | "monitor.pageErrors"
  | "monitor.networkLogs"
  | "secrets.put"
  | "secrets.delete"
  | "secrets.list"
  | "wait.url"
  | "wait.networkIdle"
  | "capture.metrics"
  | "capture.highlight"
  | "capture.elementRect";

export interface BridgeRequest {
  id: string;
  action: BridgeAction;
  params: Record<string, unknown>;
}

export interface BridgeResponse {
  id: string;
  success: boolean;
  data?: unknown;
  error?: string;
}
