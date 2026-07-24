import { LoginForm } from "@/components/LoginForm";

/// Sign-in page. Credentials go to the selected realm (the built-in "Lumen"
/// realm is the OS's own accounts); success lands on the console dashboard.
export default function LoginPage() {
  return <LoginForm />;
}
