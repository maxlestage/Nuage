# Nuage DB with the pg gem (gem install pg); Rails (ActiveRecord) and Sequel use it.
# The password is a token of an application.
#   NUAGE_TOKEN=... ruby ruby_pg.rb
require "pg"

conn = PG.connect(host: ENV.fetch("NUAGE_PG_HOST", "localhost"), port: ENV.fetch("NUAGE_PG_PORT", "5432"),
                  user: "app", dbname: "nuage", password: ENV.fetch("NUAGE_TOKEN"))
conn.exec("CREATE TABLE IF NOT EXISTS notes (id SERIAL PRIMARY KEY, text TEXT NOT NULL)")
id = conn.transaction do |tx|
  tx.exec_params("INSERT INTO notes (text) VALUES ($1) RETURNING id", ["hello from ruby pg"]).getvalue(0, 0)
end
puts "inserted #{id}"
conn.exec_params("SELECT id, text FROM notes WHERE id <= $1 ORDER BY id DESC LIMIT 5", [id]).each do |row|
  puts "#{row['id']} #{row['text']}"
end
conn.close
