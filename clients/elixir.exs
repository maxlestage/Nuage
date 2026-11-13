# Nuage DB from Elixir 1.18+ (JSON is built in; the HTTP client is Erlang's :httpc, or use
# Req / Finch in an application).
#   NUAGE_URL=https://db.example.com NUAGE_TOKEN=... elixir elixir.exs
:inets.start()
:ssl.start()

defmodule Nuage do
  @url System.get_env("NUAGE_URL", "http://localhost:8080")

  @doc "Runs SQL with ? parameters; returns %{\"columns\", \"rows\"} or %{\"rows_affected\", \"last_insert_id\"}."
  def sql(query, params \\ []) do
    headers = [{~c"authorization", ~c"Bearer " ++ String.to_charlist(System.fetch_env!("NUAGE_TOKEN"))}]
    body = JSON.encode!(%{sql: query, params: params})
    request = {String.to_charlist(@url <> "/v1/sql"), headers, ~c"application/json", body}
    {:ok, {_status, _headers, answer}} = :httpc.request(:post, request, [], body_format: :binary)

    case JSON.decode!(answer) do
      %{"error" => %{"kind" => kind, "message" => message}} -> raise "#{kind}: #{message}"
      result -> result
    end
  end
end

Nuage.sql("CREATE TABLE IF NOT EXISTS notes (id SERIAL PRIMARY KEY, text TEXT NOT NULL)")
IO.puts("inserted #{Nuage.sql("INSERT INTO notes (text) VALUES (?)", ["hello from elixir"])["last_insert_id"]}")

for row <- Nuage.sql("SELECT id, text FROM notes ORDER BY id DESC LIMIT 5")["rows"] do
  IO.puts("#{row["id"]} #{row["text"]}")
end
