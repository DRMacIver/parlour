// Talking to the server's /api endpoints. Requests are same-origin, so the
// browser sends the session cookie and an Origin header, which is what the
// server's CSRF check wants.

export class ApiError extends Error {
  constructor(
    message: string,
    readonly status: number,
  ) {
    super(message);
  }
}

async function handle<T>(response: Response): Promise<T> {
  let body: unknown = null;
  try {
    body = await response.json();
  } catch {
    // Not JSON: an error page, say.
  }
  if (!response.ok) {
    const message =
      body && typeof body === "object" && "error" in body && typeof body.error === "string"
        ? body.error
        : response.status === 429
          ? "Slow down: wait a minute, then try again."
          : "Something went wrong. Try again.";
    throw new ApiError(message, response.status);
  }
  return body as T;
}

export async function getJson<T>(url: string): Promise<T> {
  return handle<T>(
    await fetch(url, { credentials: "same-origin", headers: { Accept: "application/json" } }),
  );
}

export async function postJson<T>(url: string, body: unknown): Promise<T> {
  return handle<T>(
    await fetch(url, {
      method: "POST",
      credentials: "same-origin",
      headers: { Accept: "application/json", "Content-Type": "application/json" },
      body: JSON.stringify(body),
    }),
  );
}

export async function postForm<T>(url: string, form: FormData): Promise<T> {
  return handle<T>(
    await fetch(url, {
      method: "POST",
      credentials: "same-origin",
      headers: { Accept: "application/json" },
      body: form,
    }),
  );
}
