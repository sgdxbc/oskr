#include <gtest/gtest.h>

#include "app/Null.hpp"
#include "common/BasicClient.hpp"
#include "replication/unreplicated/Replica.hpp"
#include "transport/Simulated.hpp"

using namespace oscar;

TEST(Unreplicated, Noop)
{
    Config<SimulatedTransport> config{0, {"replica-0"}, {}};
    SimulatedTransport transport(config);
    NullApp app;
    unreplicated::Replica replica(transport, app);
}
