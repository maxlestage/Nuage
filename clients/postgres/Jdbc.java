// Nuage DB with JDBC (the PostgreSQL driver, org.postgresql:postgresql); Hibernate, Spring
// and every JVM language (Kotlin, Scala, Clojure) use it. The password is a token.
//   NUAGE_TOKEN=... java -cp postgresql.jar Jdbc.java
import java.sql.*;

public class Jdbc {
    public static void main(String[] args) throws Exception {
        String host = System.getenv().getOrDefault("NUAGE_PG_HOST", "localhost");
        String port = System.getenv().getOrDefault("NUAGE_PG_PORT", "5432");
        try (Connection c = DriverManager.getConnection("jdbc:postgresql://" + host + ":" + port + "/nuage", "app", System.getenv("NUAGE_TOKEN"))) {
            c.createStatement().execute("CREATE TABLE IF NOT EXISTS notes (id SERIAL PRIMARY KEY, text TEXT NOT NULL)");
            c.setAutoCommit(false);
            long id;
            try (PreparedStatement insert = c.prepareStatement("INSERT INTO notes (text) VALUES (?) RETURNING id")) {
                insert.setString(1, "hello from jdbc");
                try (ResultSet r = insert.executeQuery()) {
                    r.next();
                    id = r.getLong(1);
                }
            }
            c.commit();
            System.out.println("inserted " + id);
            try (PreparedStatement select = c.prepareStatement("SELECT id, text FROM notes WHERE id <= ? ORDER BY id DESC LIMIT 5")) {
                select.setLong(1, id);
                try (ResultSet r = select.executeQuery()) {
                    while (r.next()) System.out.println(r.getLong("id") + " " + r.getString("text"));
                }
            }
            c.commit();
        }
    }
}
