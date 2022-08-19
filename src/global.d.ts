export {};

declare global {
  interface Window {
    AwesomeEvent: {
      listen(eventName: string, callback: (data) => void): () => void;
    };
  }
}
