# Nuage DB from Perl 5.14+, with core modules only (HTTP::Tiny, JSON::PP; https needs
# IO::Socket::SSL).
#   NUAGE_URL=https://db.example.com NUAGE_TOKEN=... perl perl.pl
use strict;
use warnings;
use HTTP::Tiny;
use JSON::PP;

my $url = $ENV{NUAGE_URL} || 'http://localhost:8080';
my $http = HTTP::Tiny->new(default_headers => {
    'Authorization' => "Bearer $ENV{NUAGE_TOKEN}",
    'Content-Type' => 'application/json',
});

# Runs SQL with ? parameters; returns {columns, rows} or {rows_affected, last_insert_id}.
sub sql {
    my ($query, @params) = @_;
    my $res = $http->post("$url/v1/sql", { content => encode_json({ sql => $query, params => \@params }) });
    my $body = decode_json($res->{content});
    die "$body->{error}{kind}: $body->{error}{message}\n" if $body->{error};
    return $body;
}

sql('CREATE TABLE IF NOT EXISTS notes (id SERIAL PRIMARY KEY, text TEXT NOT NULL)');
print 'inserted ', sql('INSERT INTO notes (text) VALUES (?)', 'hello from perl')->{last_insert_id}, "\n";
print "$_->{id} $_->{text}\n" for @{ sql('SELECT id, text FROM notes ORDER BY id DESC LIMIT 5')->{rows} };
