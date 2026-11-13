# Nuage DB from Ruby, with the standard library only.
#   NUAGE_URL=https://db.example.com NUAGE_TOKEN=... ruby ruby.rb
require "json"
require "net/http"

URL = ENV.fetch("NUAGE_URL", "http://localhost:8080")
TOKEN = ENV.fetch("NUAGE_TOKEN")

# Runs SQL with ? parameters; returns {"columns", "rows"} or {"rows_affected", "last_insert_id"}.
def sql(query, *params)
  uri = URI("#{URL}/v1/sql")
  res = Net::HTTP.post(uri, { sql: query, params: params }.to_json,
                       "Authorization" => "Bearer #{TOKEN}", "Content-Type" => "application/json")
  body = JSON.parse(res.body)
  raise "#{body['error']['kind']}: #{body['error']['message']}" if body["error"]

  body
end

sql("CREATE TABLE IF NOT EXISTS notes (id SERIAL PRIMARY KEY, text TEXT NOT NULL)")
puts "inserted #{sql('INSERT INTO notes (text) VALUES (?)', 'hello from ruby')['last_insert_id']}"
sql("SELECT id, text FROM notes ORDER BY id DESC LIMIT 5")["rows"].each { |row| puts "#{row['id']} #{row['text']}" }
