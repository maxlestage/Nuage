# Nuage DB from Ruby, with the standard library only.
#   NUAGE_URL=https://db.example.com NUAGE_TOKEN=... ruby ruby.rb
#   Without NUAGE_TOKEN, with the server's NUAGE_ADMIN_TOKEN: it makes its own application first.
require "json"
require "net/http"

URL = ENV.fetch("NUAGE_URL", "http://localhost:8080")
$token = ENV["NUAGE_TOKEN"]

# Runs SQL with ? parameters; returns {"columns", "rows"} or {"rows_affected", "last_insert_id"}.
def sql(query, *params)
  uri = URI("#{URL}/v1/sql")
  res = Net::HTTP.post(uri, { sql: query, params: params }.to_json,
                       "Authorization" => "Bearer #{$token}", "Content-Type" => "application/json")
  body = JSON.parse(res.body)
  raise "#{body['error']['kind']}: #{body['error']['message']}" if body["error"]

  body
end

# Makes an application (a database of its own) with the management token; returns its token.
def create_app(name, admin_token)
  res = Net::HTTP.post(URI("#{URL}/api/apps"), { name: name }.to_json,
                       "Authorization" => "Bearer #{admin_token}", "Content-Type" => "application/json")
  body = JSON.parse(res.body)
  raise "#{body['error']['kind']}: #{body['error']['message']}" if body["error"]

  body["token"]
end

if $token.to_s.empty? && ENV["NUAGE_ADMIN_TOKEN"]
  $token = create_app("notes-ruby", ENV["NUAGE_ADMIN_TOKEN"])
  puts "created the application notes-ruby"
end

sql("CREATE TABLE IF NOT EXISTS notes (id SERIAL PRIMARY KEY, text TEXT NOT NULL)")
puts "inserted #{sql('INSERT INTO notes (text) VALUES (?)', 'hello from ruby')['last_insert_id']}"
sql("SELECT id, text FROM notes ORDER BY id DESC LIMIT 5")["rows"].each { |row| puts "#{row['id']} #{row['text']}" }
