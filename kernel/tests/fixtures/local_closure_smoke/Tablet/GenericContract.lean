-- Synthetic certificate context for the generic prose-launch tests.

namespace Fixture

namespace Std
abbrev I64 := Int
def ofI64 (value : Int) : I64 := value
end Std

end Fixture

postfix:max "#i64" => Fixture.Std.ofI64

namespace GenericCampaign
namespace Spec

open Fixture Fixture.Std

def observed (value : Std.I64) : Std.I64 := value

def GenericContract (value result : Std.I64) : Prop :=
  result = observed value

end Spec
end GenericCampaign
